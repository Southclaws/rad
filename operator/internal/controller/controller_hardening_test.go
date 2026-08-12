package controller

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/prometheus/client_golang/prometheus/testutil"
	dto "github.com/prometheus/client_model/go"
	appsv1 "k8s.io/api/apps/v1"
	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func TestAcquireClaimPropagatesCreateAndRaceReadErrors(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	claim := durableClaim{kind: storageClaimKind, key: storageClaim(database), conflictReason: "BucketClaimConflict"}
	name := claimName(claim.kind, claim.key)
	wantError := errors.New("storage unavailable")

	t.Run("create", func(t *testing.T) {
		reconciler, base := testReconciler(t, database)
		reconciler.Client = &interceptClient{Client: base, create: func(context.Context, client.Object, ...client.CreateOption) error {
			return wantError
		}}
		problem, err := reconciler.acquireClaim(context.Background(), database, claim, name)
		if problem != nil || !errors.Is(err, wantError) {
			t.Fatalf("acquire result = (%#v, %v)", problem, err)
		}
	})

	t.Run("already exists read", func(t *testing.T) {
		reconciler, base := testReconciler(t, database)
		reconciler.Client = &interceptClient{Client: base, create: func(context.Context, client.Object, ...client.CreateOption) error {
			return apierrors.NewAlreadyExists(schema.GroupResource{Group: "coordination.k8s.io", Resource: "leases"}, name)
		}}
		reconciler.APIReader = &scriptedReader{Reader: base, errors: []error{
			apierrors.NewNotFound(schema.GroupResource{Group: "coordination.k8s.io", Resource: "leases"}, name),
			wantError,
		}}
		problem, err := reconciler.acquireClaim(context.Background(), database, claim, name)
		if problem != nil || !errors.Is(err, wantError) {
			t.Fatalf("acquire result = (%#v, %v)", problem, err)
		}
	})
}

func TestStaleClaimCleanupRejectsUnownedAndPropagatesStorageErrors(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	stale := testClaimLease(database, routeClaimKind, "stale")
	stale.OwnerReferences = nil

	t.Run("unowned", func(t *testing.T) {
		reconciler, _ := testReconciler(t, database, stale.DeepCopy())
		if err := reconciler.removeStaleClaims(context.Background(), database, map[string]struct{}{}); err == nil {
			t.Fatal("unowned stale claim was accepted")
		}
	})

	wantError := errors.New("lease storage failed")
	t.Run("list", func(t *testing.T) {
		reconciler, base := testReconciler(t, database)
		reconciler.Client = &interceptClient{Client: base, list: func(context.Context, client.ObjectList, ...client.ListOption) error {
			return wantError
		}}
		if err := reconciler.removeStaleClaims(context.Background(), database, map[string]struct{}{}); !errors.Is(err, wantError) {
			t.Fatalf("cleanup error = %v", err)
		}
	})

	t.Run("delete", func(t *testing.T) {
		reconciler, base := testReconciler(t, database, testClaimLease(database, routeClaimKind, "stale-delete"))
		reconciler.Client = &interceptClient{Client: base, delete: func(context.Context, client.Object, ...client.DeleteOption) error {
			return wantError
		}}
		if err := reconciler.removeStaleClaims(context.Background(), database, map[string]struct{}{}); !errors.Is(err, wantError) {
			t.Fatalf("cleanup error = %v", err)
		}
	})
}

func TestClaimIndexAndClientFallbacks(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	reconciler, base := testReconciler(t, database)
	if reconciler.readClient() != base {
		t.Fatal("nil APIReader did not fall back to the controller client")
	}
	if values := claimIndexValues(&coordinationv1.Lease{}); values != nil {
		t.Fatalf("unlabelled claim index values = %v", values)
	}
	if values := claimIndexValues(testClaimLease(database, routeClaimKind, "index-values")); len(values) != 1 || values[0] != string(database.UID) {
		t.Fatalf("claim index values = %v", values)
	}

	scheme := reconciler.Scheme
	indexed := fake.NewClientBuilder().
		WithScheme(scheme).
		WithIndex(&coordinationv1.Lease{}, claimDatabaseIndex, func(object client.Object) []string {
			return []string{object.GetLabels()[claimDatabaseUID]}
		}).
		WithObjects(database, testClaimLease(database, routeClaimKind, "indexed")).
		Build()
	reconciler.Client = indexed
	reconciler.ClaimIndexAvailable = true
	if err := reconciler.removeStaleClaims(context.Background(), database, map[string]struct{}{}); err != nil {
		t.Fatal(err)
	}
}

func TestCanonicalEndpointCoversPortsPathsAndIPv6(t *testing.T) {
	for _, testcase := range []struct {
		input string
		want  string
	}{
		{" HTTP://S3.EXAMPLE:80/ ", "http://s3.example"},
		{"https://S3.EXAMPLE:443/root/?ignored=yes#fragment", "https://s3.example/root"},
		{"https://S3.EXAMPLE:9443/root", "https://s3.example:9443/root"},
		{"http://[2001:DB8::1]:80/", "http://[2001:db8::1]"},
		{"http://[2001:DB8::1]:9000/", "http://[2001:db8::1]:9000"},
		{"not a URL/", "not a url"},
	} {
		if got := canonicalEndpoint(testcase.input); got != testcase.want {
			t.Errorf("canonicalEndpoint(%q) = %q, want %q", testcase.input, got, testcase.want)
		}
	}
}

func TestConditionsDetectEveryMaterialChange(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	if !setCondition(database, "Example", metav1.ConditionFalse, "Reason", "message") {
		t.Fatal("new condition was not reported as changed")
	}
	if setCondition(database, "Example", metav1.ConditionFalse, "Reason", "message") {
		t.Fatal("identical condition was reported as changed")
	}
	for _, change := range []struct {
		status  metav1.ConditionStatus
		reason  string
		message string
	}{
		{metav1.ConditionTrue, "Reason", "message"},
		{metav1.ConditionFalse, "Other", "message"},
		{metav1.ConditionFalse, "Reason", "other"},
	} {
		if !setCondition(database, "Example", change.status, change.reason, change.message) {
			t.Fatalf("condition change %#v was ignored", change)
		}
		setCondition(database, "Example", metav1.ConditionFalse, "Reason", "message")
	}
}

func TestDependencyMetricsDistinguishStorageAndRouteClaims(t *testing.T) {
	database := testDatabase("metrics", "metrics-bucket", "metrics.rad.localhost", "metrics-s3", time.Unix(1, 0))
	reconciler := &DatabaseReconciler{}
	beforeStorage := testutil.ToFloat64(claimConflictCounter.WithLabelValues("storage"))
	beforeRoute := testutil.ToFloat64(claimConflictCounter.WithLabelValues("route"))
	reconciler.setDependencyCondition(database, radv1alpha1.ConditionClaimsAccepted, "BucketClaimConflict", "bucket")
	reconciler.setDependencyCondition(database, radv1alpha1.ConditionClaimsAccepted, "HostnameClaimConflict", "route")
	reconciler.setDependencyCondition(database, radv1alpha1.ConditionCredentialsReady, "SecretNotFound", "secret")
	if got := testutil.ToFloat64(claimConflictCounter.WithLabelValues("storage")); got != beforeStorage+1 {
		t.Fatalf("storage conflict count = %v, want %v", got, beforeStorage+1)
	}
	if got := testutil.ToFloat64(claimConflictCounter.WithLabelValues("route")); got != beforeRoute+1 {
		t.Fatalf("route conflict count = %v, want %v", got, beforeRoute+1)
	}
}

func TestTimeToReadyIsObservedOncePerDatabase(t *testing.T) {
	database := testDatabase("ttr", "ttr-bucket", "ttr.rad.localhost", "ttr-s3", time.Unix(1, 0))
	database.UID = "ttr-uid"
	database.CreationTimestamp = metav1.NewTime(time.Now().Add(-time.Minute))
	reconciler := &DatabaseReconciler{}
	before := timeToReadySampleCount(t)

	reconciler.observeTimeToReady(database)
	reconciler.observeTimeToReady(database)

	if got := timeToReadySampleCount(t); got != before+1 {
		t.Fatalf("time-to-ready samples = %d, want exactly one more than %d", got, before)
	}
}

func timeToReadySampleCount(t *testing.T) uint64 {
	t.Helper()
	var metric dto.Metric
	if err := timeToReadyHistogram.Write(&metric); err != nil {
		t.Fatal(err)
	}
	return metric.GetHistogram().GetSampleCount()
}

func TestDependencyFailureMarksDownstreamConditionsUnknown(t *testing.T) {
	database := testDatabase("unknowns", "unknowns-bucket", "unknowns.rad.localhost", "unknowns-s3", time.Unix(1, 0))
	reconciler := &DatabaseReconciler{}
	// A previously healthy database whose claims stop being satisfiable must
	// not keep asserting workload or route health it can no longer evaluate.
	setCondition(database, radv1alpha1.ConditionWorkloadReady, metav1.ConditionTrue, "Available", "was healthy")
	setCondition(database, radv1alpha1.ConditionRouteReady, metav1.ConditionTrue, "Published", "was routed")

	reconciler.setDependencyCondition(database, radv1alpha1.ConditionClaimsAccepted, "BucketClaimConflict", "bucket")

	for _, conditionType := range []string{radv1alpha1.ConditionCredentialsReady, radv1alpha1.ConditionWorkloadReady, radv1alpha1.ConditionRouteReady} {
		condition := meta.FindStatusCondition(database.Status.Conditions, conditionType)
		if condition == nil || condition.Status != metav1.ConditionUnknown {
			t.Fatalf("%s = %#v, want Unknown after an upstream failure", conditionType, condition)
		}
	}
	ready := meta.FindStatusCondition(database.Status.Conditions, radv1alpha1.ConditionReady)
	if ready == nil || ready.Status != metav1.ConditionFalse {
		t.Fatalf("Ready = %#v, want False", ready)
	}

	// A mid-chain failure leaves upstream conditions untouched.
	setCondition(database, radv1alpha1.ConditionClaimsAccepted, metav1.ConditionTrue, "Accepted", "claims fine")
	reconciler.setDependencyCondition(database, radv1alpha1.ConditionCredentialsReady, "SecretNotFound", "secret")
	claims := meta.FindStatusCondition(database.Status.Conditions, radv1alpha1.ConditionClaimsAccepted)
	if claims == nil || claims.Status != metav1.ConditionTrue {
		t.Fatalf("ClaimsAccepted = %#v, want True untouched by a downstream failure", claims)
	}
}

func TestPolicyReconciliationHandlesOwnershipAndOptionalSelectors(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	unowned := &policyv1.PodDisruptionBudget{ObjectMeta: metav1.ObjectMeta{Name: "rad-alpha", Namespace: testNamespace}}
	reconciler, _ := testReconciler(t, database, unowned)
	if err := reconciler.reconcilePodDisruptionBudget(context.Background(), database); err == nil {
		t.Fatal("unowned PodDisruptionBudget was adopted")
	}

	reconciler, kubernetesClient := testReconciler(t, database)
	reconciler.GatewayPodSelector = nil
	if err := reconciler.reconcileNetworkPolicy(context.Background(), database); err != nil {
		t.Fatal(err)
	}
	policy := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.NetworkPolicy{})
	if policy.Spec.Ingress[0].From[0].PodSelector != nil {
		t.Fatalf("unexpected gateway pod selector: %#v", policy.Spec.Ingress[0].From[0].PodSelector)
	}
	reconciler.GatewayNamespace = ""
	if err := reconciler.reconcileNetworkPolicy(context.Background(), database); err != nil {
		t.Fatal(err)
	}
	if err := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(policy), &networkingv1.NetworkPolicy{}); !apierrors.IsNotFound(err) {
		t.Fatalf("disabled NetworkPolicy remains: %v", err)
	}
}

func TestTLSValidationRejectsEveryMalformedSecretShape(t *testing.T) {
	for _, testcase := range []struct {
		name   string
		secret *corev1.Secret
	}{
		{"wrong type", &corev1.Secret{Type: corev1.SecretTypeOpaque, Data: map[string][]byte{corev1.TLSCertKey: []byte("cert"), corev1.TLSPrivateKeyKey: []byte("key")}}},
		{"missing certificate", &corev1.Secret{Type: corev1.SecretTypeTLS, Data: map[string][]byte{corev1.TLSPrivateKeyKey: []byte("key")}}},
		{"missing key", &corev1.Secret{Type: corev1.SecretTypeTLS, Data: map[string][]byte{corev1.TLSCertKey: []byte("cert")}}},
	} {
		t.Run(testcase.name, func(t *testing.T) {
			database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
			database.Spec.Route.TLSSecretName = "alpha-tls"
			testcase.secret.Name = "alpha-tls"
			testcase.secret.Namespace = testNamespace
			reconciler, _ := testReconciler(t, database, testcase.secret)
			problem, err := reconciler.validateTLS(context.Background(), database)
			if err != nil || problem == nil || problem.reason != "TLSSecretInvalid" {
				t.Fatalf("TLS validation = (%#v, %v)", problem, err)
			}
		})
	}
}

func TestReadinessReportsIndependentWorkloadAndRouteState(t *testing.T) {
	for _, testcase := range []struct {
		name          string
		workloadReady bool
		routeReady    bool
		wantReady     bool
	}{
		{"neither", false, false, false},
		{"workload only", true, false, false},
		{"route only", false, true, false},
		{"both", true, true, true},
	} {
		t.Run(testcase.name, func(t *testing.T) {
			database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
			statefulSet := &appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: "rad-alpha", Namespace: testNamespace}}
			if testcase.workloadReady {
				statefulSet.Status.ReadyReplicas = 1
				statefulSet.Status.CurrentReplicas = 1
			}
			ingress := &networkingv1.Ingress{ObjectMeta: metav1.ObjectMeta{Name: "rad-alpha", Namespace: testNamespace}}
			if testcase.routeReady {
				ingress.Status.LoadBalancer.Ingress = []networkingv1.IngressLoadBalancerIngress{{IP: "127.0.0.1"}}
			}
			reconciler, _ := testReconciler(t, database, statefulSet, ingress)
			ready, err := reconciler.observeReadiness(context.Background(), database)
			if err != nil || ready != testcase.wantReady {
				t.Fatalf("readiness = (%t, %v), want %t", ready, err, testcase.wantReady)
			}
			workload := meta.FindStatusCondition(database.Status.Conditions, radv1alpha1.ConditionWorkloadReady)
			route := meta.FindStatusCondition(database.Status.Conditions, radv1alpha1.ConditionRouteReady)
			if workload == nil || (workload.Status == metav1.ConditionTrue) != testcase.workloadReady {
				t.Fatalf("workload condition = %#v", workload)
			}
			if route == nil || (route.Status == metav1.ConditionTrue) != testcase.routeReady {
				t.Fatalf("route condition = %#v", route)
			}
			if database.Status.ObservedImage != "" {
				t.Fatalf("empty StatefulSet observed image = %q", database.Status.ObservedImage)
			}
		})
	}
}

func TestResourceAndTimingOverridesPreserveExactValues(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Resources.Requests = corev1.ResourceList{corev1.ResourceCPU: resource.MustParse("125m")}
	resources := resourcesFor(database)
	if got := resources.Requests.Cpu().String(); got != "125m" {
		t.Fatalf("custom CPU request = %s", got)
	}
	database.Spec.TerminationGracePeriodSeconds = 321
	if got := terminationGracePeriodSeconds(database); got != 321 {
		t.Fatalf("termination grace = %d", got)
	}
	database.Spec.Route.Scheme = ""
	if got := routeScheme(database); got != "https" {
		t.Fatalf("default route scheme = %q", got)
	}
	// A short grace period must not be spent entirely on withdrawing traffic;
	// Rad still has to close Slate before Kubernetes kills the container.
	database.Spec.TerminationGracePeriodSeconds = 10
	if got := shutdownDrainMilliseconds(database); got != 5_000 {
		t.Fatalf("drain within a ten second grace period = %dms, want half of it", got)
	}
	if got := closeTimeoutMilliseconds(database); got != 5_000 {
		t.Fatalf("close timeout under a ten second grace period = %dms, want the floor", got)
	}
	database.Spec.TerminationGracePeriodSeconds = 0
	if got := shutdownDrainMilliseconds(database); got != 15_000 {
		t.Fatalf("default drain = %dms, want one readiness cycle", got)
	}
	if got := closeTimeoutMilliseconds(database); got != 100_000 {
		t.Fatalf("default close timeout = %dms, want grace minus drain minus margin", got)
	}
	reconciler := &DatabaseReconciler{
		RequeueInterval:         3 * time.Second,
		DependencyPollInterval:  4 * time.Second,
		CredentialPollInterval:  5 * time.Second,
		MaxConcurrentReconciles: 6,
	}
	if reconciler.requeueInterval() != 3*time.Second || reconciler.dependencyPollInterval() != 4*time.Second || reconciler.credentialPollInterval() != 5*time.Second || reconciler.maxConcurrentReconciles() != 6 {
		t.Fatal("controller timing overrides were not preserved")
	}
}

func TestDerivedResourceNamesPreserveBoundaryLengthNames(t *testing.T) {
	resourceBoundary := strings.Repeat("a", 59)
	if got := resourceName(resourceBoundary); got != "rad-"+resourceBoundary || len(got) != 63 {
		t.Fatalf("boundary resource name = %q", got)
	}
	headlessBoundary := strings.Repeat("b", 50)
	if got := headlessServiceName(headlessBoundary); got != "rad-"+headlessBoundary+"-headless" || len(got) != 63 {
		t.Fatalf("boundary headless name = %q", got)
	}
}

func TestReconcileErrorsIncrementTheMetric(t *testing.T) {
	reconciler, base := testReconciler(t)
	wantError := errors.New("API unavailable")
	reconciler.Client = &interceptClient{Client: base, get: func(context.Context, client.ObjectKey, client.Object, ...client.GetOption) error {
		return wantError
	}}
	before := testutil.ToFloat64(reconcileErrorCounter)
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: types.NamespacedName{Namespace: testNamespace, Name: "missing"}}); !errors.Is(err, wantError) {
		t.Fatalf("reconcile error = %v", err)
	}
	if got := testutil.ToFloat64(reconcileErrorCounter); got != before+1 {
		t.Fatalf("reconcile error count = %v, want %v", got, before+1)
	}
}

type interceptClient struct {
	client.Client
	get    func(context.Context, client.ObjectKey, client.Object, ...client.GetOption) error
	list   func(context.Context, client.ObjectList, ...client.ListOption) error
	create func(context.Context, client.Object, ...client.CreateOption) error
	delete func(context.Context, client.Object, ...client.DeleteOption) error
}

func (c *interceptClient) Get(ctx context.Context, key client.ObjectKey, object client.Object, options ...client.GetOption) error {
	if c.get != nil {
		return c.get(ctx, key, object, options...)
	}
	return c.Client.Get(ctx, key, object, options...)
}

func (c *interceptClient) List(ctx context.Context, list client.ObjectList, options ...client.ListOption) error {
	if c.list != nil {
		return c.list(ctx, list, options...)
	}
	return c.Client.List(ctx, list, options...)
}

func (c *interceptClient) Create(ctx context.Context, object client.Object, options ...client.CreateOption) error {
	if c.create != nil {
		return c.create(ctx, object, options...)
	}
	return c.Client.Create(ctx, object, options...)
}

func (c *interceptClient) Delete(ctx context.Context, object client.Object, options ...client.DeleteOption) error {
	if c.delete != nil {
		return c.delete(ctx, object, options...)
	}
	return c.Client.Delete(ctx, object, options...)
}

type scriptedReader struct {
	client.Reader
	errors []error
	reads  int
}

func (r *scriptedReader) Get(ctx context.Context, key client.ObjectKey, object client.Object, options ...client.GetOption) error {
	if r.reads < len(r.errors) {
		err := r.errors[r.reads]
		r.reads++
		return err
	}
	return r.Reader.Get(ctx, key, object, options...)
}
