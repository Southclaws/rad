package controller

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"sort"
	"strconv"
	"sync"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/apimachinery/pkg/util/intstr"
	"k8s.io/client-go/tools/record"
	"k8s.io/utils/ptr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
	"sigs.k8s.io/controller-runtime/pkg/reconcile"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

const (
	finalizerName              = "radengine.dev/runtime"
	credentialsVersionKey      = "radengine.dev/credentials-version"
	workloadIdentityVersionKey = "radengine.dev/workload-identity-version"
	roleLabel                  = "radengine.dev/role"
	writeRole                  = "write"
	readRole                   = "read"
	publicPort                 = 7237
	adminPort                  = 7238
	internalPort               = 7239
	// The unprivileged user the Rad container runs as, and the group that
	// owns anything mounted for it to read.
	radRuntimeUser            = 65532
	readinessPeriodSeconds    = 5
	readinessFailureThreshold = 3
	defaultRequeueInterval    = time.Duration(2_000_000_000)
	defaultDependencyInterval = time.Duration(30_000_000_000)
	defaultCredentialInterval = time.Minute
	defaultMaxConcurrent      = 4
	reconciliationTimeout     = time.Duration(120_000_000_000)
)

var requiredCredentialKeys = []string{"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"}

// DatabaseReconciler makes one single-writer Rad runtime match each
// Database. External S3 state remains outside Kubernetes and is always
// retained by this API version.
type DatabaseReconciler struct {
	client.Client
	APIReader               client.Reader
	Scheme                  *runtime.Scheme
	Recorder                record.EventRecorder
	RadImage                string
	IngressClass            string
	GatewayNamespace        string
	GatewayPodSelector      *metav1.LabelSelector
	RequeueInterval         time.Duration
	DependencyPollInterval  time.Duration
	CredentialPollInterval  time.Duration
	MaxConcurrentReconciles int
	ClaimIndexAvailable     bool
	// CertManager answers whether certificates can be requested here. Nil
	// means they cannot, which is the correct answer for a fake client.
	CertManager CertManagerPresence

	// Databases whose creation-to-Ready duration was already observed by this
	// controller instance, keyed by UID.
	readyObserved sync.Map
}

// +kubebuilder:rbac:groups=radengine.dev,resources=databases,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=radengine.dev,resources=databases/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=radengine.dev,resources=databases/finalizers,verbs=update
// +kubebuilder:rbac:groups=apps,resources=statefulsets;deployments,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=serviceaccounts,verbs=get;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=secrets,verbs=get;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=events,verbs=create;patch
// +kubebuilder:rbac:groups=networking.k8s.io,resources=ingresses;networkpolicies,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=policy,resources=poddisruptionbudgets,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=coordination.k8s.io,resources=leases,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=cert-manager.io,resources=issuers;certificates,verbs=get;list;watch;create;update;patch;delete

func (r *DatabaseReconciler) Reconcile(ctx context.Context, request ctrl.Request) (result ctrl.Result, err error) {
	defer func() {
		if err != nil {
			reconcileErrorCounter.Inc()
		}
	}()
	database := &radv1alpha1.Database{}
	if err = r.Get(ctx, request.NamespacedName, database); err != nil {
		if apierrors.IsNotFound(err) {
			databaseReadyGauge.DeleteLabelValues(request.Namespace, request.Name)
		}
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	if !database.DeletionTimestamp.IsZero() {
		return r.reconcileDelete(ctx, database)
	}

	if !controllerutil.ContainsFinalizer(database, finalizerName) {
		before := database.DeepCopy()
		controllerutil.AddFinalizer(database, finalizerName)
		return ctrl.Result{}, r.Patch(ctx, database, client.MergeFrom(before))
	}

	problem, err := r.reconcileClaims(ctx, database)
	if err != nil {
		return ctrl.Result{}, err
	}
	if problem != nil {
		if err := r.quiesceRejectedDatabase(ctx, database); err != nil {
			return ctrl.Result{}, err
		}
		setQuiescingConditions(database, problem.reason, problem.message)
		return r.waitWithStatus(ctx, database, radv1alpha1.ConditionClaimsAccepted, problem.reason, problem.message)
	}
	setCondition(database, radv1alpha1.ConditionClaimsAccepted, metav1.ConditionTrue, "Accepted", "bucket and hostname claims are exclusive")

	authentication, problem, err := r.resolveAuthentication(ctx, database)
	if err != nil {
		return ctrl.Result{}, err
	}
	if problem != nil {
		if err := r.quiesceRejectedDatabase(ctx, database); err != nil {
			return ctrl.Result{}, err
		}
		setQuiescingConditions(database, problem.reason, problem.message)
		return r.waitWithStatus(ctx, database, radv1alpha1.ConditionCredentialsReady, problem.reason, problem.message)
	}
	setCondition(database, radv1alpha1.ConditionCredentialsReady, metav1.ConditionTrue, "Available", authentication.message)
	tlsProblem, err := r.validateTLS(ctx, database)
	if err != nil {
		return ctrl.Result{}, err
	}
	if tlsProblem != nil {
		if err := r.deleteOwnedIfPresent(ctx, database, &networkingv1.Ingress{ObjectMeta: metav1.ObjectMeta{
			Name: resourceName(database.Name), Namespace: database.Namespace,
		}}); err != nil {
			return ctrl.Result{}, err
		}
		return r.waitWithStatus(ctx, database, radv1alpha1.ConditionRouteReady, tlsProblem.reason, tlsProblem.message)
	}

	transport, err := r.resolveInternalTLS(ctx, database)
	if err != nil {
		return ctrl.Result{}, err
	}
	// Required mode must also stop a workload that already serves plaintext.
	// Returning before reconciliation is sufficient only for a new database.
	if !transport.ready && internalTLSMode(database) == radv1alpha1.InternalTLSRequired {
		if err := r.quiesceRejectedDatabase(ctx, database); err != nil {
			return ctrl.Result{}, err
		}
		setCondition(database, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionFalse,
			"InternalTLSUnavailable", transport.pending)
		return r.waitWithStatus(ctx, database, radv1alpha1.ConditionInternalTLSReady,
			"InternalTLSUnavailable", transport.pending)
	}
	publishInternalTLSCondition(database, transport)

	if err := r.reconcileResources(ctx, database, authentication, transport); err != nil {
		setCondition(database, radv1alpha1.ConditionReady, metav1.ConditionFalse, "ReconcileFailed", err.Error())
		_ = r.patchStatus(ctx, database)
		return ctrl.Result{}, err
	}
	database.Status.InternalTransport = transportStatus(database, transport)

	ready, err := r.observeReadiness(ctx, database)
	if err != nil {
		return ctrl.Result{}, err
	}
	if err := r.patchStatus(ctx, database); err != nil {
		return ctrl.Result{}, err
	}
	if !ready {
		return ctrl.Result{RequeueAfter: r.requeueInterval()}, nil
	}
	return ctrl.Result{RequeueAfter: r.credentialPollInterval()}, nil
}

func (r *DatabaseReconciler) SetupWithManager(manager ctrl.Manager) error {
	if err := manager.GetFieldIndexer().IndexField(context.Background(), &coordinationv1.Lease{}, claimDatabaseIndex, claimIndexValues); err != nil {
		return fmt.Errorf("index claim Leases: %w", err)
	}
	r.ClaimIndexAvailable = true
	return ctrl.NewControllerManagedBy(manager).
		For(&radv1alpha1.Database{}).
		Owns(&appsv1.StatefulSet{}).
		Owns(&appsv1.Deployment{}).
		Owns(&corev1.Service{}).
		Owns(&networkingv1.Ingress{}).
		Owns(&networkingv1.NetworkPolicy{}).
		Owns(&policyv1.PodDisruptionBudget{}).
		Owns(&coordinationv1.Lease{}).
		WithOptions(controller.Options{
			MaxConcurrentReconciles: r.maxConcurrentReconciles(),
			ReconciliationTimeout:   reconciliationTimeout,
		}).
		Complete(r)
}

type dependencyProblem struct {
	reason  string
	message string
}

type resolvedAuthentication struct {
	serviceAccountName string
	secretName         string
	versionAnnotation  string
	message            string
}

func (r *DatabaseReconciler) resolveAuthentication(
	ctx context.Context,
	database *radv1alpha1.Database,
) (resolvedAuthentication, *dependencyProblem, error) {
	authentication := database.Spec.Storage.Authentication
	if authentication.CredentialsSecretRef != nil {
		secret := &corev1.Secret{}
		key := types.NamespacedName{Namespace: database.Namespace, Name: authentication.CredentialsSecretRef.Name}
		if err := r.readClient().Get(ctx, key, secret); err != nil {
			if apierrors.IsNotFound(err) {
				return resolvedAuthentication{}, &dependencyProblem{
					reason:  "SecretNotFound",
					message: fmt.Sprintf("credential Secret %q does not exist", key.Name),
				}, nil
			}
			return resolvedAuthentication{}, nil, err
		}
		for _, key := range requiredCredentialKeys {
			if len(secret.Data[key]) == 0 {
				return resolvedAuthentication{}, &dependencyProblem{
					reason:  "SecretInvalid",
					message: fmt.Sprintf("credential Secret %q is missing %s", secret.Name, key),
				}, nil
			}
		}
		return resolvedAuthentication{
			serviceAccountName: resourceName(database.Name),
			secretName:         secret.Name,
			versionAnnotation:  objectVersion(secret),
			message:            fmt.Sprintf("using credential Secret %q", secret.Name),
		}, nil, nil
	}

	if authentication.ServiceAccountName != nil {
		serviceAccount := &corev1.ServiceAccount{}
		key := types.NamespacedName{Namespace: database.Namespace, Name: *authentication.ServiceAccountName}
		if err := r.readClient().Get(ctx, key, serviceAccount); err != nil {
			if apierrors.IsNotFound(err) {
				return resolvedAuthentication{}, &dependencyProblem{
					reason:  "ServiceAccountNotFound",
					message: fmt.Sprintf("workload identity ServiceAccount %q does not exist", key.Name),
				}, nil
			}
			return resolvedAuthentication{}, nil, err
		}
		return resolvedAuthentication{
			serviceAccountName: serviceAccount.Name,
			versionAnnotation:  objectVersion(serviceAccount),
			message:            fmt.Sprintf("using workload identity ServiceAccount %q", serviceAccount.Name),
		}, nil, nil
	}

	return resolvedAuthentication{}, &dependencyProblem{
		reason:  "AuthenticationInvalid",
		message: "exactly one S3 authentication source is required",
	}, nil
}

func (r *DatabaseReconciler) reconcileResources(
	ctx context.Context,
	database *radv1alpha1.Database,
	authentication resolvedAuthentication,
	transport internalTransport,
) error {
	if authentication.secretName != "" {
		if err := r.reconcileManagedServiceAccount(ctx, database); err != nil {
			return err
		}
	} else if authentication.serviceAccountName != resourceName(database.Name) {
		if err := r.deleteOwnedIfPresent(ctx, database, &corev1.ServiceAccount{ObjectMeta: metav1.ObjectMeta{
			Name: resourceName(database.Name), Namespace: database.Namespace,
		}}); err != nil {
			return err
		}
	}
	if err := r.reconcileHeadlessService(ctx, database); err != nil {
		return err
	}
	if err := r.reconcileFrontendService(ctx, database); err != nil {
		return err
	}
	if err := r.reconcilePodDisruptionBudget(ctx, database); err != nil {
		return err
	}
	if err := r.reconcileNetworkPolicy(ctx, database); err != nil {
		return err
	}
	if err := r.reconcileRelay(ctx, database); err != nil {
		return err
	}
	if err := r.reconcileStatefulSet(ctx, database, authentication, transport); err != nil {
		return err
	}
	if err := r.reconcileReaders(ctx, database, authentication, transport); err != nil {
		return err
	}
	return r.reconcileIngress(ctx, database)
}

func (r *DatabaseReconciler) validateTLS(
	ctx context.Context,
	database *radv1alpha1.Database,
) (*dependencyProblem, error) {
	if database.Spec.Route.TLSSecretName == "" {
		return nil, nil
	}
	secret := &corev1.Secret{}
	key := types.NamespacedName{Namespace: database.Namespace, Name: database.Spec.Route.TLSSecretName}
	if err := r.readClient().Get(ctx, key, secret); err != nil {
		if apierrors.IsNotFound(err) {
			return &dependencyProblem{reason: "TLSSecretNotFound", message: fmt.Sprintf("TLS Secret %q does not exist", key.Name)}, nil
		}
		return nil, err
	}
	if secret.Type != corev1.SecretTypeTLS || len(secret.Data[corev1.TLSCertKey]) == 0 || len(secret.Data[corev1.TLSPrivateKeyKey]) == 0 {
		return &dependencyProblem{reason: "TLSSecretInvalid", message: fmt.Sprintf("TLS Secret %q must contain tls.crt and tls.key", key.Name)}, nil
	}
	return nil, nil
}

func (r *DatabaseReconciler) reconcileManagedServiceAccount(ctx context.Context, database *radv1alpha1.Database) error {
	account := &corev1.ServiceAccount{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, account, func() error {
		if err := r.prepareOwned(database, account); err != nil {
			return err
		}
		account.Labels = labelsFor(database)
		account.AutomountServiceAccountToken = ptr.To(false)
		return nil
	})
	return err
}

func (r *DatabaseReconciler) reconcileHeadlessService(ctx context.Context, database *radv1alpha1.Database) error {
	service := &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: headlessServiceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, service, func() error {
		if err := r.prepareOwned(database, service); err != nil {
			return err
		}
		service.Labels = labelsFor(database)
		service.Spec.Type = corev1.ServiceTypeClusterIP
		service.Spec.ClusterIP = corev1.ClusterIPNone
		service.Spec.Selector = roleSelectorLabelsFor(database, writeRole)
		service.Spec.Ports = []corev1.ServicePort{{Name: "http", Port: publicPort, TargetPort: intstrFromInt(publicPort), Protocol: corev1.ProtocolTCP}}
		return nil
	})
	return err
}

func (r *DatabaseReconciler) reconcileFrontendService(ctx context.Context, database *radv1alpha1.Database) error {
	service := &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, service, func() error {
		if err := r.prepareOwned(database, service); err != nil {
			return err
		}
		service.Labels = labelsFor(database)
		service.Spec.Type = corev1.ServiceTypeClusterIP
		service.Spec.Selector = roleSelectorLabelsFor(database, writeRole)
		service.Spec.Ports = []corev1.ServicePort{{Name: "http", Port: 80, TargetPort: intstrFromInt(publicPort), Protocol: corev1.ProtocolTCP, AppProtocol: ptr.To("http")}}
		return nil
	})
	return err
}

func (r *DatabaseReconciler) reconcileStatefulSet(
	ctx context.Context,
	database *radv1alpha1.Database,
	authentication resolvedAuthentication,
	transport internalTransport,
) error {
	statefulSet := &appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}}
	operation, err := r.createOrUpdate(ctx, statefulSet, func() error {
		if err := r.prepareOwned(database, statefulSet); err != nil {
			return err
		}
		statefulSet.Labels = roleLabelsFor(database, writeRole)
		statefulSet.Spec.Replicas = ptr.To[int32](1)
		statefulSet.Spec.RevisionHistoryLimit = ptr.To[int32](3)
		statefulSet.Spec.MinReadySeconds = 5
		statefulSet.Spec.ServiceName = headlessServiceName(database.Name)
		statefulSet.Spec.PodManagementPolicy = appsv1.OrderedReadyPodManagement
		statefulSet.Spec.UpdateStrategy = appsv1.StatefulSetUpdateStrategy{Type: appsv1.RollingUpdateStatefulSetStrategyType}
		statefulSet.Spec.Selector = &metav1.LabelSelector{MatchLabels: roleSelectorLabelsFor(database, writeRole)}

		statefulSet.Spec.Template = corev1.PodTemplateSpec{
			ObjectMeta: metav1.ObjectMeta{
				Labels:      roleLabelsFor(database, writeRole),
				Annotations: rolloutAnnotations(authentication),
			},
			Spec: r.radPodSpec(database, authentication, transport, writeRole),
		}
		return nil
	})
	if err == nil && operation == controllerutil.OperationResultUpdated {
		r.event(database, corev1.EventTypeNormal, "WriterUpdated", "Rad writer configuration updated")
	}
	return err
}

// Only the writer serves the internal API, so only the writer declares its
// port. A reader reaches it; nothing reaches a reader on it.
func radContainerPorts(database *radv1alpha1.Database, role string) []corev1.ContainerPort {
	ports := []corev1.ContainerPort{{Name: "http", ContainerPort: publicPort, Protocol: corev1.ProtocolTCP}}
	if relayEnabled(database) && role == writeRole {
		ports = append(ports, corev1.ContainerPort{
			Name: "internal", ContainerPort: internalPort, Protocol: corev1.ProtocolTCP,
		})
	}
	return ports
}

// Readers address the writer by Service name, which is one of the names the
// certificate is issued for, so verification succeeds without any reader
// knowing a pod identity.
func writerInternalURL(database *radv1alpha1.Database, transport internalTransport) string {
	scheme := "http"
	if transport.tls() {
		scheme = "https"
	}
	return fmt.Sprintf(
		"%s://%s.%s.svc:%d",
		scheme, relayResourceName(database.Name), database.Namespace, internalPort,
	)
}

// A rollout annotation changes whenever the credential source does, so
// rotating a Secret or a ServiceAccount replaces the pods that read it.
func rolloutAnnotations(authentication resolvedAuthentication) map[string]string {
	if authentication.secretName != "" {
		return map[string]string{credentialsVersionKey: authentication.versionAnnotation}
	}
	return map[string]string{workloadIdentityVersionKey: authentication.versionAnnotation}
}

func (r *DatabaseReconciler) radPodSpec(
	database *radv1alpha1.Database,
	authentication resolvedAuthentication,
	transport internalTransport,
	role string,
) corev1.PodSpec {
	container := corev1.Container{
		Name:            "rad",
		Image:           r.RadImage,
		ImagePullPolicy: corev1.PullIfNotPresent,
		Args:            []string{"serve"},
		// Rad prints its terminal reason (fencing, unusable storage) as
		// its last log line, and the non-root container cannot write the
		// kubelet's termination-log file; falling back to the log surfaces
		// that reason in the container's terminated state.
		TerminationMessagePolicy: corev1.TerminationMessageFallbackToLogsOnError,
		Ports:                    radContainerPorts(database, role),
		Env:                      databaseEnvironment(database, transport, role),
		Resources:                resourcesFor(database),
		SecurityContext: &corev1.SecurityContext{
			AllowPrivilegeEscalation: ptr.To(false),
			ReadOnlyRootFilesystem:   ptr.To(true),
			RunAsNonRoot:             ptr.To(true),
			RunAsUser:                ptr.To[int64](radRuntimeUser),
			Capabilities:             &corev1.Capabilities{Drop: []corev1.Capability{"ALL"}},
		},
		StartupProbe:   startupProbe(),
		ReadinessProbe: readinessProbe(),
		LivenessProbe:  livenessProbe(),
	}
	automountToken := (*bool)(nil)
	if authentication.secretName != "" {
		container.Env = append(container.Env, credentialEnvironment(authentication.secretName)...)
		automountToken = ptr.To(false)
	}
	var volumes []corev1.Volume
	if relayEnabled(database) {
		container.VolumeMounts = append(container.VolumeMounts, relayVolumeMount())
		volumes = append(volumes, relayVolume(database))
		if transport.tls() {
			container.VolumeMounts = append(container.VolumeMounts, relayTLSVolumeMount(role))
			volumes = append(volumes, relayTLSVolume(transport, role))
		}
	}
	return corev1.PodSpec{
		Volumes:                       volumes,
		ServiceAccountName:            authentication.serviceAccountName,
		AutomountServiceAccountToken:  automountToken,
		EnableServiceLinks:            ptr.To(false),
		TerminationGracePeriodSeconds: ptr.To(terminationGracePeriodSeconds(database)),
		SecurityContext: &corev1.PodSecurityContext{
			RunAsNonRoot: ptr.To(true),
			// Mounted Secret files are owned by root. Without a group the
			// runtime user shares, a non-root process cannot read its own
			// token however permissive the file mode is.
			FSGroup:        ptr.To[int64](radRuntimeUser),
			SeccompProfile: &corev1.SeccompProfile{Type: corev1.SeccompProfileTypeRuntimeDefault},
		},
		Containers: []corev1.Container{container},
	}
}

func (r *DatabaseReconciler) reconcileIngress(ctx context.Context, database *radv1alpha1.Database) error {
	ingress := &networkingv1.Ingress{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, ingress, func() error {
		if err := r.prepareOwned(database, ingress); err != nil {
			return err
		}
		ingress.Labels = labelsFor(database)
		ingress.Spec = networkingv1.IngressSpec{
			IngressClassName: optionalString(r.IngressClass),
			Rules: []networkingv1.IngressRule{{
				Host: database.Spec.Route.Hostname,
				IngressRuleValue: networkingv1.IngressRuleValue{HTTP: &networkingv1.HTTPIngressRuleValue{Paths: []networkingv1.HTTPIngressPath{{
					Path:     "/",
					PathType: ptr.To(networkingv1.PathTypePrefix),
					Backend: networkingv1.IngressBackend{Service: &networkingv1.IngressServiceBackend{
						Name: resourceName(database.Name),
						Port: networkingv1.ServiceBackendPort{Number: 80},
					}},
				}}}},
			}},
		}
		if database.Spec.Route.TLSSecretName != "" {
			ingress.Spec.TLS = []networkingv1.IngressTLS{{
				Hosts:      []string{database.Spec.Route.Hostname},
				SecretName: database.Spec.Route.TLSSecretName,
			}}
		}
		return nil
	})
	return err
}

func (r *DatabaseReconciler) observeReadiness(ctx context.Context, database *radv1alpha1.Database) (bool, error) {
	statefulSet := &appsv1.StatefulSet{}
	if err := r.Get(ctx, types.NamespacedName{Namespace: database.Namespace, Name: resourceName(database.Name)}, statefulSet); err != nil {
		return false, err
	}
	ingress := &networkingv1.Ingress{}
	if err := r.Get(ctx, types.NamespacedName{Namespace: database.Namespace, Name: resourceName(database.Name)}, ingress); err != nil {
		return false, err
	}

	if err := r.observeReaders(ctx, database); err != nil {
		return false, err
	}

	workloadReady := statefulSet.Status.ReadyReplicas == 1 && statefulSet.Status.CurrentReplicas == 1
	if workloadReady {
		setCondition(database, radv1alpha1.ConditionWorkloadReady, metav1.ConditionTrue, "Available", "the single Rad writer is ready")
	} else {
		setCondition(database, radv1alpha1.ConditionWorkloadReady, metav1.ConditionFalse, "Progressing", "waiting for the single Rad writer")
	}
	routeReady := len(ingress.Status.LoadBalancer.Ingress) > 0
	if routeReady {
		setCondition(database, radv1alpha1.ConditionRouteReady, metav1.ConditionTrue, "Published", "the shared ingress accepted the tenant hostname")
	} else {
		setCondition(database, radv1alpha1.ConditionRouteReady, metav1.ConditionFalse, "Progressing", "waiting for an ingress address")
	}

	database.Status.ObservedGeneration = database.Generation
	database.Status.URL = routeScheme(database) + "://" + database.Spec.Route.Hostname
	database.Status.ServiceName = resourceName(database.Name)
	database.Status.StatefulSetName = resourceName(database.Name)
	database.Status.ReadyReplicas = statefulSet.Status.ReadyReplicas
	database.Status.DesiredImage = r.RadImage
	if len(statefulSet.Spec.Template.Spec.Containers) > 0 {
		database.Status.ObservedImage = statefulSet.Spec.Template.Spec.Containers[0].Image
	}
	ready := workloadReady && routeReady
	if ready {
		if setCondition(database, radv1alpha1.ConditionReady, metav1.ConditionTrue, "Available", "database is ready") {
			r.event(database, corev1.EventTypeNormal, "DatabaseReady", "Rad database is ready at %s", database.Status.URL)
			r.observeTimeToReady(database)
		}
		databaseReadyGauge.WithLabelValues(database.Namespace, database.Name).Set(1)
	} else {
		setCondition(database, radv1alpha1.ConditionReady, metav1.ConditionFalse, "Progressing", "database is not ready yet")
		databaseReadyGauge.WithLabelValues(database.Namespace, database.Name).Set(0)
	}
	return ready, nil
}

func (r *DatabaseReconciler) reconcileDelete(ctx context.Context, database *radv1alpha1.Database) (ctrl.Result, error) {
	if !controllerutil.ContainsFinalizer(database, finalizerName) {
		return ctrl.Result{}, nil
	}

	routeRemoved, err := r.deleteOwnedAndWait(ctx, database, &networkingv1.Ingress{ObjectMeta: metav1.ObjectMeta{
		Name: resourceName(database.Name), Namespace: database.Namespace,
	}})
	if err != nil {
		return ctrl.Result{}, err
	}
	if !routeRemoved {
		return ctrl.Result{RequeueAfter: r.requeueInterval()}, nil
	}
	routeClaimsRemoved, err := r.deleteClaimsByKindAndWait(ctx, database, routeClaimKind)
	if err != nil {
		return ctrl.Result{}, err
	}
	if !routeClaimsRemoved {
		return ctrl.Result{RequeueAfter: r.requeueInterval()}, nil
	}

	// Readers go before the writer: read traffic stops first, and a reader
	// holds nothing that must be drained.
	objects := readerObjects(database)
	objects = append(objects, relayObjects(database)...)
	// Operator-created cert-manager resources are owned and removed. A
	// user-supplied Secret is never in this list: the operator validated it,
	// it did not create it.
	if r.CertManager != nil && r.CertManager.Available(ctx) {
		objects = append(objects, certManagerObjects(database)...)
	}
	objects = append(objects,
		&networkingv1.NetworkPolicy{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}},
		&policyv1.PodDisruptionBudget{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}},
		&appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}},
		&corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}},
		&corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: headlessServiceName(database.Name), Namespace: database.Namespace}},
	)
	for _, object := range objects {
		done, err := r.deleteOwnedAndWait(ctx, database, object)
		if err != nil {
			return ctrl.Result{}, err
		}
		if !done {
			return ctrl.Result{RequeueAfter: r.requeueInterval()}, nil
		}
	}
	serviceAccountRemoved, err := r.deleteManagedServiceAccountAndWait(ctx, database)
	if err != nil {
		return ctrl.Result{}, err
	}
	if !serviceAccountRemoved {
		return ctrl.Result{RequeueAfter: r.requeueInterval()}, nil
	}
	storageClaimsRemoved, err := r.deleteClaimsByKindAndWait(ctx, database, storageClaimKind)
	if err != nil {
		return ctrl.Result{}, err
	}
	if !storageClaimsRemoved {
		return ctrl.Result{RequeueAfter: r.requeueInterval()}, nil
	}

	before := database.DeepCopy()
	controllerutil.RemoveFinalizer(database, finalizerName)
	databaseReadyGauge.DeleteLabelValues(database.Namespace, database.Name)
	r.event(database, corev1.EventTypeNormal, "DatabaseDeleted", "Managed Kubernetes resources removed; S3 data retained")
	return ctrl.Result{}, r.Patch(ctx, database, client.MergeFrom(before))
}

func (r *DatabaseReconciler) deleteClaimsByKindAndWait(
	ctx context.Context,
	database *radv1alpha1.Database,
	kind string,
) (bool, error) {
	var leases coordinationv1.LeaseList
	options := []client.ListOption{client.InNamespace(database.Namespace)}
	if r.ClaimIndexAvailable {
		options = append(options, client.MatchingFields{claimDatabaseIndex: string(database.UID)})
	} else {
		options = append(options, client.MatchingLabels{claimDatabaseUID: string(database.UID)})
	}
	if err := r.List(ctx, &leases, options...); err != nil {
		return false, err
	}
	for index := range leases.Items {
		lease := &leases.Items[index]
		if lease.Labels[claimKindLabel] != kind {
			continue
		}
		done, err := r.deleteOwnedAndWait(ctx, database, lease)
		if err != nil || !done {
			return false, err
		}
	}
	return true, nil
}

func (r *DatabaseReconciler) deleteManagedServiceAccountAndWait(
	ctx context.Context,
	database *radv1alpha1.Database,
) (bool, error) {
	account := &corev1.ServiceAccount{}
	key := types.NamespacedName{Namespace: database.Namespace, Name: resourceName(database.Name)}
	if err := r.Get(ctx, key, account); err != nil {
		if apierrors.IsNotFound(err) {
			return true, nil
		}
		return false, err
	}
	if !metav1.IsControlledBy(account, database) {
		return true, nil
	}
	return r.deleteOwnedAndWait(ctx, database, account)
}

func (r *DatabaseReconciler) deleteOwnedAndWait(ctx context.Context, database *radv1alpha1.Database, object client.Object) (bool, error) {
	key := client.ObjectKeyFromObject(object)
	if err := r.Get(ctx, key, object); err != nil {
		if apierrors.IsNotFound(err) {
			return true, nil
		}
		return false, err
	}
	if !metav1.IsControlledBy(object, database) {
		return false, fmt.Errorf("refusing to delete unowned %T %s", object, key)
	}
	policy := metav1.DeletePropagationForeground
	if err := r.Delete(ctx, object, &client.DeleteOptions{PropagationPolicy: &policy}); err != nil && !apierrors.IsNotFound(err) {
		return false, err
	}
	return false, nil
}

func (r *DatabaseReconciler) prepareOwned(database *radv1alpha1.Database, object client.Object) error {
	if object.GetResourceVersion() != "" && !metav1.IsControlledBy(object, database) {
		return fmt.Errorf("refusing to adopt existing %T %s/%s", object, object.GetNamespace(), object.GetName())
	}
	return controllerutil.SetControllerReference(database, object, r.Scheme)
}

func (r *DatabaseReconciler) waitWithStatus(
	ctx context.Context,
	database *radv1alpha1.Database,
	conditionType string,
	reason string,
	message string,
) (ctrl.Result, error) {
	r.setDependencyCondition(database, conditionType, reason, message)
	database.Status.ObservedGeneration = database.Generation
	if err := r.patchStatus(ctx, database); err != nil {
		return ctrl.Result{}, err
	}
	return ctrl.Result{RequeueAfter: r.dependencyPollInterval()}, nil
}

func (r *DatabaseReconciler) patchStatus(ctx context.Context, database *radv1alpha1.Database) error {
	latest := &radv1alpha1.Database{}
	key := client.ObjectKeyFromObject(database)
	if err := r.Get(ctx, key, latest); err != nil {
		return err
	}
	before := latest.DeepCopy()
	latest.Status = database.Status
	return r.Status().Patch(ctx, latest, client.MergeFrom(before))
}

func publishInternalTLSCondition(database *radv1alpha1.Database, transport internalTransport) {
	switch {
	case !relayEnabled(database):
		setCondition(database, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionTrue,
			"NotApplicable", "no readers, so no statistics channel to secure")
	case transport.tls() && transport.ready:
		setCondition(database, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionTrue,
			"Available", "the statistics channel is encrypted")
	case transport.tls():
		setCondition(database, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionFalse,
			"Provisioning", transport.pending)
	default:
		setCondition(database, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionFalse,
			"Plaintext", "the statistics channel is authenticated but not encrypted")
	}
}

func transportStatus(
	database *radv1alpha1.Database,
	transport internalTransport,
) *radv1alpha1.InternalTransportStatus {
	if !relayEnabled(database) {
		return nil
	}
	mode := "plaintext"
	if transport.tls() {
		mode = "tls"
	}
	return &radv1alpha1.InternalTransportStatus{
		Mode:              mode,
		Provider:          transport.provider(),
		CertificateSecret: transport.secret,
		WriterAddress:     writerInternalURL(database, transport),
	}
}

func setQuiescingConditions(database *radv1alpha1.Database, reason, message string) {
	setCondition(database, radv1alpha1.ConditionWorkloadReady, metav1.ConditionFalse, reason, "writer quiesced: "+message)
	setCondition(database, radv1alpha1.ConditionRouteReady, metav1.ConditionFalse, reason, "route quiesced: "+message)
}

func databaseEnvironment(
	database *radv1alpha1.Database,
	transport internalTransport,
	role string,
) []corev1.EnvVar {
	values := map[string]string{
		"RAD_ADDR": fmt.Sprintf("0.0.0.0:%d", publicPort),
		// Loopback keeps the admin surface off the pod network; it stays
		// reachable through kubectl port-forward, which enters the pod's own
		// network namespace.
		"RAD_ADMIN_ADDR":        fmt.Sprintf("127.0.0.1:%d", adminPort),
		"RAD_CATALOG_MODE":      string(database.Spec.CatalogMode),
		"RAD_CLOSE_TIMEOUT_MS":  strconv.FormatInt(closeTimeoutMilliseconds(database), 10),
		"RAD_ROLE":              role,
		"RAD_S3_BUCKET":         database.Spec.Storage.Bucket,
		"RAD_S3_PREFIX":         database.Spec.Storage.Prefix,
		"RAD_S3_REGION":         database.Spec.Storage.Region,
		"RAD_SHUTDOWN_DRAIN_MS": strconv.FormatInt(shutdownDrainMilliseconds(database), 10),
		"RAD_STORAGE":           "s3",
	}
	if relayEnabled(database) {
		values["RAD_RELAY_TOKEN_FILE"] = relayTokenPath()
		if role == writeRole {
			values["RAD_INTERNAL_ADDR"] = fmt.Sprintf("0.0.0.0:%d", internalPort)
			if transport.tls() {
				values["RAD_INTERNAL_TLS_CERT"] = relayCertificatePath()
				values["RAD_INTERNAL_TLS_KEY"] = relayCertificateKeyPath()
			}
		} else {
			values["RAD_RELAY_TARGET"] = writerInternalURL(database, transport)
			if transport.tls() {
				values["RAD_RELAY_CA"] = relayAuthorityPath()
			}
		}
	}
	if database.Spec.CaptureWorkloadCorpus {
		values["RAD_CAPTURE_WORKLOAD_CORPUS"] = "true"
	}
	if database.Spec.Storage.Endpoint != "" {
		values["RAD_S3_ENDPOINT"] = database.Spec.Storage.Endpoint
	}
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	environment := make([]corev1.EnvVar, 0, len(keys))
	for _, key := range keys {
		environment = append(environment, corev1.EnvVar{Name: key, Value: values[key]})
	}
	return environment
}

func credentialEnvironment(secretName string) []corev1.EnvVar {
	required := false
	optional := true
	return []corev1.EnvVar{
		{
			Name: "AWS_ACCESS_KEY_ID",
			ValueFrom: &corev1.EnvVarSource{SecretKeyRef: &corev1.SecretKeySelector{
				LocalObjectReference: corev1.LocalObjectReference{Name: secretName},
				Key:                  "AWS_ACCESS_KEY_ID",
				Optional:             &required,
			}},
		},
		{
			Name: "AWS_SECRET_ACCESS_KEY",
			ValueFrom: &corev1.EnvVarSource{SecretKeyRef: &corev1.SecretKeySelector{
				LocalObjectReference: corev1.LocalObjectReference{Name: secretName},
				Key:                  "AWS_SECRET_ACCESS_KEY",
				Optional:             &required,
			}},
		},
		{
			Name: "AWS_SESSION_TOKEN",
			ValueFrom: &corev1.EnvVarSource{SecretKeyRef: &corev1.SecretKeySelector{
				LocalObjectReference: corev1.LocalObjectReference{Name: secretName},
				Key:                  "AWS_SESSION_TOKEN",
				Optional:             &optional,
			}},
		},
	}
}

func resourcesFor(database *radv1alpha1.Database) corev1.ResourceRequirements {
	resources := database.Spec.Resources
	if len(resources.Requests) == 0 && len(resources.Limits) == 0 {
		resources.Requests = corev1.ResourceList{
			corev1.ResourceCPU:    resource.MustParse("50m"),
			corev1.ResourceMemory: resource.MustParse("128Mi"),
		}
		resources.Limits = corev1.ResourceList{
			corev1.ResourceCPU:    resource.MustParse("500m"),
			corev1.ResourceMemory: resource.MustParse("512Mi"),
		}
	}
	return resources
}

// Rad answers /startupz only once storage is open, the catalog's identity and
// mode are validated, and its background workers run. The window is generous
// because a cold object store, not the database, dominates first start.
func startupProbe() *corev1.Probe {
	return &corev1.Probe{
		ProbeHandler:     corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{Path: "/startupz", Port: intstrFromInt(publicPort)}},
		PeriodSeconds:    5,
		TimeoutSeconds:   5,
		FailureThreshold: 120,
	}
}

// /readyz withdraws the pod from the Service while the writer is fenced,
// storage is degraded, or the process is draining.
func readinessProbe() *corev1.Probe {
	return &corev1.Probe{
		ProbeHandler:     corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{Path: "/readyz", Port: intstrFromInt(publicPort)}},
		PeriodSeconds:    readinessPeriodSeconds,
		TimeoutSeconds:   5,
		FailureThreshold: readinessFailureThreshold,
	}
}

// /livez ignores storage, so an object-store outage withdraws traffic through
// readiness instead of restarting a process that would return to that outage.
func livenessProbe() *corev1.Probe {
	return &corev1.Probe{
		ProbeHandler:     corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{Path: "/livez", Port: intstrFromInt(publicPort)}},
		PeriodSeconds:    10,
		TimeoutSeconds:   5,
		FailureThreshold: 12,
	}
}

// Rad keeps serving while readiness reports unavailable for this long, so the
// endpoints controller can move traffic elsewhere before the listeners stop.
// One full readiness cycle is enough to be observed, and capping the window at
// half the grace period leaves room for an orderly Slate close.
func shutdownDrainMilliseconds(database *radv1alpha1.Database) int64 {
	drain := int64(readinessPeriodSeconds * readinessFailureThreshold)
	if half := terminationGracePeriodSeconds(database) / 2; drain > half {
		drain = half
	}
	return drain * 1000
}

// Bound the orderly Slate close so a writer whose storage is unreachable exits
// with a message inside the grace period instead of hanging into SIGKILL. The
// budget is what remains of the grace period after the readiness drain, less a
// margin for the exit itself.
func closeTimeoutMilliseconds(database *radv1alpha1.Database) int64 {
	remaining := terminationGracePeriodSeconds(database)*1000 - shutdownDrainMilliseconds(database) - 5_000
	return max(remaining, 5_000)
}

func terminationGracePeriodSeconds(database *radv1alpha1.Database) int64 {
	if database.Spec.TerminationGracePeriodSeconds > 0 {
		return database.Spec.TerminationGracePeriodSeconds
	}
	return 120
}

func routeScheme(database *radv1alpha1.Database) string {
	if database.Spec.Route.Scheme != "" {
		return database.Spec.Route.Scheme
	}
	return "https"
}

func labelsFor(database *radv1alpha1.Database) map[string]string {
	labels := selectorLabelsFor(database)
	labels["app.kubernetes.io/managed-by"] = "rad-operator"
	labels["app.kubernetes.io/part-of"] = "rad"
	return labels
}

func selectorLabelsFor(database *radv1alpha1.Database) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":     "rad",
		"app.kubernetes.io/instance": resourceName(database.Name),
		"radengine.dev/database":     database.Name,
	}
}

// Pod selectors must name a role. Two workloads share the database labels, and
// a selector that matched both would let each workload's controller act on the
// other's pods.
func roleSelectorLabelsFor(database *radv1alpha1.Database, role string) map[string]string {
	labels := selectorLabelsFor(database)
	labels[roleLabel] = role
	return labels
}

func roleLabelsFor(database *radv1alpha1.Database, role string) map[string]string {
	labels := labelsFor(database)
	labels[roleLabel] = role
	return labels
}

func resourceName(databaseName string) string {
	const prefix = "rad-"
	name := prefix + databaseName
	if len(name) <= 63 {
		return name
	}
	digest := sha256.Sum256([]byte(databaseName))
	suffix := hex.EncodeToString(digest[:4])
	return name[:54] + "-" + suffix
}

func headlessServiceName(databaseName string) string {
	return suffixedResourceName(databaseName, "-headless")
}

func readerResourceName(databaseName string) string {
	return suffixedResourceName(databaseName, "-reader")
}

func relayResourceName(databaseName string) string {
	return suffixedResourceName(databaseName, "-internal")
}

func suffixedResourceName(databaseName string, suffix string) string {
	name := resourceName(databaseName)
	if len(name)+len(suffix) <= 63 {
		return name + suffix
	}
	digest := sha256.Sum256([]byte(databaseName + suffix))
	return name[:54] + "-" + hex.EncodeToString(digest[:4])
}

func objectVersion(object client.Object) string {
	return string(object.GetUID()) + "/" + object.GetResourceVersion()
}

func optionalString(value string) *string {
	if value == "" {
		return nil
	}
	return ptr.To(value)
}

func intstrFromInt(value int32) intstr.IntOrString {
	return intstr.FromInt32(value)
}

func (r *DatabaseReconciler) requeueInterval() time.Duration {
	if r.RequeueInterval > 0 {
		return r.RequeueInterval
	}
	return defaultRequeueInterval
}

func (r *DatabaseReconciler) dependencyPollInterval() time.Duration {
	if r.DependencyPollInterval > 0 {
		return r.DependencyPollInterval
	}
	return defaultDependencyInterval
}

func (r *DatabaseReconciler) credentialPollInterval() time.Duration {
	if r.CredentialPollInterval > 0 {
		return r.CredentialPollInterval
	}
	return defaultCredentialInterval
}

func (r *DatabaseReconciler) maxConcurrentReconciles() int {
	if r.MaxConcurrentReconciles > 0 {
		return r.MaxConcurrentReconciles
	}
	return defaultMaxConcurrent
}

var _ reconcile.Reconciler = (*DatabaseReconciler)(nil)
