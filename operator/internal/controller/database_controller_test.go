package controller

import (
	"context"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/utils/ptr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

const testNamespace = "default"

func TestReconcileBuildsOneIsolatedRadRuntime(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	secret := testSecret("alpha-s3")
	reconciler, kubernetesClient := testReconciler(t, database, secret)
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	if got := ptr.Deref(statefulSet.Spec.Replicas, 0); got != 1 {
		t.Fatalf("replicas = %d, want exactly one", got)
	}
	if statefulSet.Spec.ServiceName != "rad-alpha-headless" {
		t.Fatalf("serviceName = %q", statefulSet.Spec.ServiceName)
	}
	container := statefulSet.Spec.Template.Spec.Containers[0]
	if container.Image != "rad:test" {
		t.Fatalf("image = %q", container.Image)
	}
	environment := environmentMap(container.Env)
	for key, want := range map[string]string{
		"RAD_ROLE":         "write",
		"RAD_STORAGE":      "s3",
		"RAD_S3_BUCKET":    "alpha-bucket",
		"RAD_S3_PREFIX":    "rad",
		"RAD_S3_REGION":    "us-east-1",
		"RAD_S3_ENDPOINT":  "http://rustfs:9000",
		"RAD_CATALOG_MODE": "schema",
		// Loopback: the admin surface must not be reachable from the pod
		// network, only through port-forward.
		"RAD_ADMIN_ADDR": "127.0.0.1:7238",
		// One full readiness cycle, so the endpoints controller observes the
		// withdrawal before Rad stops listening.
		"RAD_SHUTDOWN_DRAIN_MS": "15000",
		// Grace period minus drain minus exit margin: 120s - 15s - 5s.
		"RAD_CLOSE_TIMEOUT_MS": "100000",
	} {
		if got := environment[key]; got != want {
			t.Errorf("%s = %q, want %q", key, got, want)
		}
	}
	if len(container.EnvFrom) != 0 {
		t.Fatalf("credential envFrom = %#v, want no broad Secret projection", container.EnvFrom)
	}
	for _, key := range []string{"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN"} {
		variable := findEnvironmentVariable(t, container.Env, key)
		if variable.ValueFrom == nil || variable.ValueFrom.SecretKeyRef == nil || variable.ValueFrom.SecretKeyRef.Name != "alpha-s3" || variable.ValueFrom.SecretKeyRef.Key != key {
			t.Fatalf("%s projection = %#v", key, variable)
		}
	}
	if container.StartupProbe == nil || container.ReadinessProbe == nil || container.LivenessProbe == nil {
		t.Fatal("Rad container does not have startup, readiness, and liveness probes")
	}
	if container.TerminationMessagePolicy != corev1.TerminationMessageFallbackToLogsOnError {
		t.Fatalf("terminationMessagePolicy = %q, want the terminal log line surfaced", container.TerminationMessagePolicy)
	}
	// Each probe must ask Rad its own question. Sharing one endpoint cannot
	// distinguish a process needing a restart from one that should only stop
	// receiving traffic.
	for _, probe := range []struct {
		name  string
		probe *corev1.Probe
		path  string
	}{
		{"startup", container.StartupProbe, "/startupz"},
		{"readiness", container.ReadinessProbe, "/readyz"},
		{"liveness", container.LivenessProbe, "/livez"},
	} {
		if probe.probe.HTTPGet == nil || probe.probe.HTTPGet.Path != probe.path {
			t.Errorf("%s probe = %#v, want an HTTP GET of %s", probe.name, probe.probe, probe.path)
		}
		if probe.probe.HTTPGet != nil && probe.probe.HTTPGet.Port != intstrFromInt(publicPort) {
			t.Errorf("%s probe port = %#v, want the public port", probe.name, probe.probe.HTTPGet.Port)
		}
	}
	if container.StartupProbe.FailureThreshold*container.StartupProbe.PeriodSeconds < 600 {
		t.Fatalf("startup probe window = %#v, want at least ten minutes", container.StartupProbe)
	}
	if container.LivenessProbe.FailureThreshold*container.LivenessProbe.PeriodSeconds < 120 {
		t.Fatalf("liveness failure window = %#v, want at least two minutes", container.LivenessProbe)
	}
	if ptr.Deref(statefulSet.Spec.Template.Spec.AutomountServiceAccountToken, true) {
		t.Fatal("static credential pod automounts a Kubernetes API token")
	}

	frontend := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &corev1.Service{})
	if frontend.Spec.Type != corev1.ServiceTypeClusterIP || frontend.Spec.ClusterIP == corev1.ClusterIPNone {
		t.Fatalf("frontend service is not an ordinary ClusterIP: %#v", frontend.Spec)
	}
	headless := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-headless"}, &corev1.Service{})
	if headless.Spec.ClusterIP != corev1.ClusterIPNone {
		t.Fatalf("headless clusterIP = %q", headless.Spec.ClusterIP)
	}
	ingress := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.Ingress{})
	if ptr.Deref(ingress.Spec.IngressClassName, "") != "traefik" || ingress.Spec.Rules[0].Host != "alpha.rad.localhost" {
		t.Fatalf("ingress = %#v", ingress.Spec)
	}
	if ingress.Spec.Rules[0].HTTP.Paths[0].Backend.Service.Name != "rad-alpha" {
		t.Fatalf("ingress backend = %#v", ingress.Spec.Rules[0].HTTP.Paths[0].Backend)
	}
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &policyv1.PodDisruptionBudget{})
	networkPolicy := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.NetworkPolicy{})
	if got := networkPolicy.Spec.Ingress[0].From[0].NamespaceSelector.MatchLabels["kubernetes.io/metadata.name"]; got != "gateway-system" {
		t.Fatalf("network policy gateway namespace = %q", got)
	}
}

func TestEachDatabaseUsesItsOwnBucketAndSecret(t *testing.T) {
	alpha := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	beta := testDatabase("beta", "beta-bucket", "beta.rad.localhost", "beta-s3", time.Unix(2, 0))
	reconciler, kubernetesClient := testReconciler(t, alpha, beta, testSecret("alpha-s3"), testSecret("beta-s3"))
	reconcileReadySpec(t, reconciler, alpha)
	reconcileReadySpec(t, reconciler, beta)

	for _, testcase := range []struct {
		name   string
		bucket string
		secret string
	}{{"alpha", "alpha-bucket", "alpha-s3"}, {"beta", "beta-bucket", "beta-s3"}} {
		statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-" + testcase.name}, &appsv1.StatefulSet{})
		container := statefulSet.Spec.Template.Spec.Containers[0]
		if got := environmentMap(container.Env)["RAD_S3_BUCKET"]; got != testcase.bucket {
			t.Errorf("%s bucket = %q", testcase.name, got)
		}
		if got := findEnvironmentVariable(t, container.Env, "AWS_ACCESS_KEY_ID").ValueFrom.SecretKeyRef.Name; got != testcase.secret {
			t.Errorf("%s Secret = %q", testcase.name, got)
		}
	}
}

func TestMissingAndMalformedSecretsDoNotStartRad(t *testing.T) {
	for _, testcase := range []struct {
		name    string
		objects []client.Object
		reason  string
	}{
		{name: "missing", reason: "SecretNotFound"},
		{
			name: "malformed",
			objects: []client.Object{&corev1.Secret{
				ObjectMeta: metav1.ObjectMeta{Name: "alpha-s3", Namespace: testNamespace},
				Data:       map[string][]byte{"AWS_ACCESS_KEY_ID": []byte("key")},
			}},
			reason: "SecretInvalid",
		},
	} {
		t.Run(testcase.name, func(t *testing.T) {
			database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
			objects := append([]client.Object{database}, testcase.objects...)
			reconciler, kubernetesClient := testReconciler(t, objects...)
			reconcileReadySpec(t, reconciler, database)

			if err := kubernetesClient.Get(context.Background(), types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{}); err == nil {
				t.Fatal("Rad StatefulSet exists without valid credentials")
			}
			observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
			condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionReady)
			if condition == nil || condition.Status != metav1.ConditionFalse || condition.Reason != testcase.reason {
				t.Fatalf("Ready condition = %#v", condition)
			}
		})
	}
}

func TestFirstDatabaseAtomicallyOwnsBucketAndHostnameClaims(t *testing.T) {
	owner := testDatabase("alpha", "shared-bucket", "shared.rad.localhost", "alpha-s3", time.Unix(1, 0))
	conflict := testDatabase("beta", "shared-bucket", "beta.rad.localhost", "beta-s3", time.Unix(2, 0))
	reconciler, kubernetesClient := testReconciler(t, owner, conflict, testSecret("alpha-s3"), testSecret("beta-s3"))
	reconcileReadySpec(t, reconciler, owner)
	reconcileReadySpec(t, reconciler, conflict)

	if err := kubernetesClient.Get(context.Background(), types.NamespacedName{Namespace: testNamespace, Name: "rad-beta"}, &appsv1.StatefulSet{}); err == nil {
		t.Fatal("conflicting bucket started a Rad writer")
	}
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(conflict), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionClaimsAccepted)
	if condition == nil || condition.Reason != "BucketClaimConflict" {
		t.Fatalf("claim condition = %#v", condition)
	}

	observed.Spec.Storage.Bucket = "beta-bucket"
	observed.Spec.Route.Hostname = owner.Spec.Route.Hostname
	if err := kubernetesClient.Update(context.Background(), observed); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(conflict)}); err != nil {
		t.Fatal(err)
	}
	observed = getObject(t, kubernetesClient, client.ObjectKeyFromObject(conflict), &radv1alpha1.Database{})
	condition = meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionClaimsAccepted)
	if condition == nil || condition.Reason != "HostnameClaimConflict" {
		t.Fatalf("hostname claim condition = %#v", condition)
	}
}

func TestClaimNamesAreStableAndDoNotExposeStorageIdentity(t *testing.T) {
	first := claimName(storageClaimKind, "https://s3.example\x00private-bucket")
	second := claimName(storageClaimKind, "https://s3.example\x00private-bucket")
	if first != second || len(first) > 63 {
		t.Fatalf("claim name = %q then %q", first, second)
	}
	if first == claimName(routeClaimKind, "https://s3.example\x00private-bucket") {
		t.Fatal("claim kind was not included in the durable Lease name")
	}
}

func TestStorageClaimCanonicalizesEquivalentEndpoints(t *testing.T) {
	alpha := testDatabase("alpha", "shared-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	beta := testDatabase("beta", "SHARED-BUCKET", "beta.rad.localhost", "beta-s3", time.Unix(1, 0))
	alpha.Spec.Storage.Endpoint = "HTTPS://S3.EXAMPLE:443/"
	beta.Spec.Storage.Endpoint = "https://s3.example"
	if got, want := storageClaim(alpha), storageClaim(beta); got != want {
		t.Fatalf("equivalent storage claims differ: %q != %q", got, want)
	}
}

func TestServiceAccountAuthenticationLeavesCredentialsToWorkloadIdentity(t *testing.T) {
	serviceAccountName := "alpha-workload"
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "", time.Unix(1, 0))
	database.Spec.Storage.Authentication = radv1alpha1.S3Authentication{ServiceAccountName: &serviceAccountName}
	serviceAccount := &corev1.ServiceAccount{ObjectMeta: metav1.ObjectMeta{Name: serviceAccountName, Namespace: testNamespace, UID: "sa-uid", ResourceVersion: "7"}}
	reconciler, kubernetesClient := testReconciler(t, database, serviceAccount)
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	if statefulSet.Spec.Template.Spec.ServiceAccountName != serviceAccountName {
		t.Fatalf("service account = %q", statefulSet.Spec.Template.Spec.ServiceAccountName)
	}
	if len(statefulSet.Spec.Template.Spec.Containers[0].EnvFrom) != 0 {
		t.Fatal("workload identity pod received a static credential Secret")
	}
	if statefulSet.Spec.Template.Spec.AutomountServiceAccountToken != nil {
		t.Fatal("operator disabled the projected token needed by workload identity")
	}
	if _, exists := statefulSet.Spec.Template.Annotations[workloadIdentityVersionKey]; !exists {
		t.Fatal("workload identity changes do not roll the StatefulSet")
	}
}

func TestCredentialSecretRotationRollsTheStatefulSetTemplate(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	secret := testSecret("alpha-s3")
	reconciler, kubernetesClient := testReconciler(t, database, secret)
	reconcileReadySpec(t, reconciler, database)
	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	before := statefulSet.Spec.Template.Annotations[credentialsVersionKey]

	secret = getObject(t, kubernetesClient, client.ObjectKeyFromObject(secret), &corev1.Secret{})
	secret.Data["AWS_SECRET_ACCESS_KEY"] = []byte("rotated")
	if err := kubernetesClient.Update(context.Background(), secret); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}
	statefulSet = getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	after := statefulSet.Spec.Template.Annotations[credentialsVersionKey]
	if before == after {
		t.Fatalf("credential version did not change: %q", before)
	}
}

func TestCredentialRemovalQuiescesTheWriterAndRoute(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	secret := testSecret("alpha-s3")
	reconciler, kubernetesClient := testReconciler(t, database, secret)
	reconcileReadySpec(t, reconciler, database)

	if err := kubernetesClient.Delete(context.Background(), secret); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}
	for _, object := range []client.Object{&appsv1.StatefulSet{}, &networkingv1.Ingress{}} {
		if err := kubernetesClient.Get(context.Background(), types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, object); !apierrors.IsNotFound(err) {
			t.Fatalf("%T remains after credential removal: %v", object, err)
		}
	}
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionCredentialsReady)
	if condition == nil || condition.Reason != "SecretNotFound" {
		t.Fatalf("CredentialsReady condition = %#v", condition)
	}
}

func TestAuthenticationCanMoveFromStaticCredentialsToWorkloadIdentity(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	serviceAccount := &corev1.ServiceAccount{ObjectMeta: metav1.ObjectMeta{
		Name: "alpha-workload", Namespace: testNamespace, UID: "workload-uid", ResourceVersion: "1",
	}}
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"), serviceAccount)
	reconcileReadySpec(t, reconciler, database)
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &corev1.ServiceAccount{})

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	observed.Spec.Storage.Authentication = radv1alpha1.S3Authentication{ServiceAccountName: &serviceAccount.Name}
	if err := kubernetesClient.Update(context.Background(), observed); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}

	if err := kubernetesClient.Get(context.Background(), types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &corev1.ServiceAccount{}); !apierrors.IsNotFound(err) {
		t.Fatalf("managed static-credential ServiceAccount remains after rotation: %v", err)
	}
	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	if statefulSet.Spec.Template.Spec.ServiceAccountName != serviceAccount.Name {
		t.Fatalf("service account = %q", statefulSet.Spec.Template.Spec.ServiceAccountName)
	}
	if variable := findEnvironmentVariableOptional(statefulSet.Spec.Template.Spec.Containers[0].Env, "AWS_ACCESS_KEY_ID"); variable != nil {
		t.Fatalf("static credential remains after rotation: %#v", variable)
	}
}

func TestTLSSecretMustExistBeforePublishingIngress(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Route.Scheme = "https"
	database.Spec.Route.TLSSecretName = "alpha-tls"
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	if err := kubernetesClient.Get(context.Background(), types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.Ingress{}); !apierrors.IsNotFound(err) {
		t.Fatalf("Ingress exists without TLS Secret: %v", err)
	}
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionRouteReady)
	if condition == nil || condition.Reason != "TLSSecretNotFound" {
		t.Fatalf("RouteReady condition = %#v", condition)
	}

	tlsSecret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: "alpha-tls", Namespace: testNamespace},
		Type:       corev1.SecretTypeTLS,
		Data: map[string][]byte{
			corev1.TLSCertKey:       []byte("certificate"),
			corev1.TLSPrivateKeyKey: []byte("private-key"),
		},
	}
	if err := kubernetesClient.Create(context.Background(), tlsSecret); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}
	ingress := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.Ingress{})
	if len(ingress.Spec.TLS) != 1 || ingress.Spec.TLS[0].SecretName != tlsSecret.Name {
		t.Fatalf("Ingress TLS = %#v", ingress.Spec.TLS)
	}
}

func TestReadyRequiresBothWriterAndSharedIngress(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	statefulSet.Status.ReadyReplicas = 1
	statefulSet.Status.CurrentReplicas = 1
	if err := kubernetesClient.Status().Update(context.Background(), statefulSet); err != nil {
		t.Fatal(err)
	}
	ingress := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.Ingress{})
	ingress.Status.LoadBalancer.Ingress = []networkingv1.IngressLoadBalancerIngress{{IP: "172.18.0.5"}}
	if err := kubernetesClient.Status().Update(context.Background(), ingress); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionReady)
	if condition == nil || condition.Status != metav1.ConditionTrue || condition.Reason != "Available" {
		t.Fatalf("Ready condition = %#v", condition)
	}
	if observed.Status.URL != "http://alpha.rad.localhost" || observed.Status.ReadyReplicas != 1 {
		t.Fatalf("status = %#v", observed.Status)
	}
}

func TestDeleteRefusesResourcesTheControllerDoesNotOwn(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	service := &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: "rad-alpha", Namespace: testNamespace}}
	reconciler, kubernetesClient := testReconciler(t, database, service)
	done, err := reconciler.deleteOwnedAndWait(context.Background(), database, &corev1.Service{ObjectMeta: service.ObjectMeta})
	if err == nil || done {
		t.Fatalf("delete unowned resource = (%v, %v)", done, err)
	}
	if getErr := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(service), &corev1.Service{}); getErr != nil {
		t.Fatalf("unowned service was deleted: %v", getErr)
	}
}

func TestDeletionRemovesRouteFirstAndRetainsExternalIdentity(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "", time.Unix(1, 0))
	serviceAccountName := "alpha-workload"
	database.Spec.Storage.Authentication = radv1alpha1.S3Authentication{ServiceAccountName: &serviceAccountName}
	database.Finalizers = []string{finalizerName}
	database.DeletionTimestamp = &metav1.Time{Time: time.Unix(2, 0)}
	owner := databaseOwnerReference(database)

	ingress := &networkingv1.Ingress{ObjectMeta: ownedObjectMeta("rad-alpha", owner)}
	statefulSet := &appsv1.StatefulSet{ObjectMeta: ownedObjectMeta("rad-alpha", owner)}
	frontend := &corev1.Service{ObjectMeta: ownedObjectMeta("rad-alpha", owner)}
	headless := &corev1.Service{ObjectMeta: ownedObjectMeta("rad-alpha-headless", owner)}
	externalIdentity := &corev1.ServiceAccount{ObjectMeta: metav1.ObjectMeta{Name: serviceAccountName, Namespace: testNamespace}}
	routeClaim := testClaimLease(database, routeClaimKind, "current-route")
	staleRouteClaim := testClaimLease(database, routeClaimKind, "stale-route")
	storageClaim := testClaimLease(database, storageClaimKind, "storage")
	reconciler, kubernetesClient := testReconciler(
		t, database, ingress, statefulSet, frontend, headless, externalIdentity,
		routeClaim, staleRouteClaim, storageClaim,
	)

	if _, err := reconciler.reconcileDelete(context.Background(), database); err != nil {
		t.Fatal(err)
	}
	if err := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(ingress), &networkingv1.Ingress{}); !apierrors.IsNotFound(err) {
		t.Fatalf("route was not removed first: %v", err)
	}
	if err := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(statefulSet), &appsv1.StatefulSet{}); err != nil {
		t.Fatalf("writer was removed before the route: %v", err)
	}

	for attempt := 0; controllerutil.ContainsFinalizer(database, finalizerName) && attempt < 10; attempt++ {
		if _, err := reconciler.reconcileDelete(context.Background(), database); err != nil {
			t.Fatal(err)
		}
	}
	if controllerutil.ContainsFinalizer(database, finalizerName) {
		t.Fatal("database finalizer remained after all managed resources were removed")
	}
	if err := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(externalIdentity), &corev1.ServiceAccount{}); err != nil {
		t.Fatalf("external workload identity was not retained: %v", err)
	}
	var leases coordinationv1.LeaseList
	if err := kubernetesClient.List(context.Background(), &leases, client.InNamespace(testNamespace), client.MatchingLabels{claimDatabaseUID: string(database.UID)}); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) != 0 {
		t.Fatalf("owned claim Leases remain after deletion: %#v", leases.Items)
	}
}

func TestResourceNamesRemainDNSLabels(t *testing.T) {
	name := "tenant-with-a-name-that-is-deliberately-longer-than-a-derived-resource-allows"
	first := resourceName(name)
	second := resourceName(name)
	if len(first) > 63 || first != second {
		t.Fatalf("resource name = %q", first)
	}
	if got := headlessServiceName(name); len(got) > 63 {
		t.Fatalf("headless service name = %q", got)
	}
}

func TestDefaultRequeueInterval(t *testing.T) {
	reconciler := &DatabaseReconciler{}
	if got := reconciler.requeueInterval(); got != 2*time.Second {
		t.Fatalf("default readiness interval = %s", got)
	}
	if got := reconciler.dependencyPollInterval(); got != 30*time.Second {
		t.Fatalf("default dependency interval = %s", got)
	}
	if got := reconciler.credentialPollInterval(); got != time.Minute {
		t.Fatalf("default credential interval = %s", got)
	}
	if got := reconciler.maxConcurrentReconciles(); got != 4 {
		t.Fatalf("default concurrency = %d", got)
	}
}

func testReconciler(t *testing.T, objects ...client.Object) (*DatabaseReconciler, client.Client) {
	t.Helper()
	scheme := runtime.NewScheme()
	if err := corev1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := appsv1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := coordinationv1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := networkingv1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := policyv1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := radv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	kubernetesClient := fake.NewClientBuilder().
		WithScheme(scheme).
		WithStatusSubresource(&radv1alpha1.Database{}, &appsv1.StatefulSet{}, &networkingv1.Ingress{}).
		WithObjects(objects...).
		Build()
	return &DatabaseReconciler{
		Client:                  kubernetesClient,
		APIReader:               kubernetesClient,
		Scheme:                  scheme,
		RadImage:                "rad:test",
		IngressClass:            "traefik",
		GatewayNamespace:        "gateway-system",
		GatewayPodSelector:      &metav1.LabelSelector{MatchLabels: map[string]string{"app": "gateway"}},
		RequeueInterval:         time.Millisecond,
		DependencyPollInterval:  time.Millisecond,
		CredentialPollInterval:  time.Millisecond,
		MaxConcurrentReconciles: 2,
	}, kubernetesClient
}

func reconcileReadySpec(t *testing.T, reconciler *DatabaseReconciler, database *radv1alpha1.Database) {
	t.Helper()
	request := ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}
	for range 2 {
		if _, err := reconciler.Reconcile(context.Background(), request); err != nil {
			t.Fatal(err)
		}
	}
}

func testDatabase(name, bucket, hostname, secret string, created time.Time) *radv1alpha1.Database {
	return &radv1alpha1.Database{
		TypeMeta: metav1.TypeMeta{APIVersion: radv1alpha1.GroupVersion.String(), Kind: "Database"},
		ObjectMeta: metav1.ObjectMeta{
			Name:              name,
			Namespace:         testNamespace,
			UID:               types.UID(name + "-uid"),
			CreationTimestamp: metav1.NewTime(created),
			Generation:        1,
		},
		Spec: radv1alpha1.DatabaseSpec{
			Storage: radv1alpha1.S3Storage{
				Bucket:   bucket,
				Prefix:   "rad",
				Region:   "us-east-1",
				Endpoint: "http://rustfs:9000",
				Authentication: radv1alpha1.S3Authentication{
					CredentialsSecretRef: &corev1.LocalObjectReference{Name: secret},
				},
			},
			CatalogMode:                   radv1alpha1.CatalogModeSchema,
			Route:                         radv1alpha1.Route{Hostname: hostname, Scheme: "http"},
			DeletionPolicy:                radv1alpha1.DeletionPolicyRetain,
			TerminationGracePeriodSeconds: 120,
		},
	}
}

func testSecret(name string) *corev1.Secret {
	return &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: testNamespace, UID: types.UID(name + "-uid"), ResourceVersion: "1"},
		Data: map[string][]byte{
			"AWS_ACCESS_KEY_ID":     []byte("access-key"),
			"AWS_SECRET_ACCESS_KEY": []byte("secret-key"),
		},
	}
}

func databaseOwnerReference(database *radv1alpha1.Database) metav1.OwnerReference {
	return metav1.OwnerReference{
		APIVersion:         radv1alpha1.GroupVersion.String(),
		Kind:               "Database",
		Name:               database.Name,
		UID:                database.UID,
		Controller:         ptr.To(true),
		BlockOwnerDeletion: ptr.To(true),
	}
}

func ownedObjectMeta(name string, owner metav1.OwnerReference) metav1.ObjectMeta {
	return metav1.ObjectMeta{Name: name, Namespace: testNamespace, OwnerReferences: []metav1.OwnerReference{owner}}
}

func testClaimLease(database *radv1alpha1.Database, kind, key string) *coordinationv1.Lease {
	holder := claimHolder(database)
	return &coordinationv1.Lease{
		ObjectMeta: metav1.ObjectMeta{
			Name:            claimName(kind, key),
			Namespace:       database.Namespace,
			OwnerReferences: []metav1.OwnerReference{databaseOwnerReference(database)},
			Labels: map[string]string{
				claimKindLabel:   kind,
				claimDatabaseUID: string(database.UID),
			},
		},
		Spec: coordinationv1.LeaseSpec{HolderIdentity: &holder},
	}
}

func getObject[T client.Object](t *testing.T, kubernetesClient client.Client, key types.NamespacedName, object T) T {
	t.Helper()
	if err := kubernetesClient.Get(context.Background(), key, object); err != nil {
		t.Fatal(err)
	}
	return object
}

func environmentMap(environment []corev1.EnvVar) map[string]string {
	result := make(map[string]string, len(environment))
	for _, variable := range environment {
		result[variable.Name] = variable.Value
	}
	return result
}

func findEnvironmentVariableOptional(environment []corev1.EnvVar, name string) *corev1.EnvVar {
	for index := range environment {
		if environment[index].Name == name {
			return &environment[index]
		}
	}
	return nil
}

func findEnvironmentVariable(t *testing.T, environment []corev1.EnvVar, name string) corev1.EnvVar {
	t.Helper()
	if variable := findEnvironmentVariableOptional(environment, name); variable != nil {
		return *variable
	}
	t.Fatalf("environment variable %s not found", name)
	return corev1.EnvVar{}
}
