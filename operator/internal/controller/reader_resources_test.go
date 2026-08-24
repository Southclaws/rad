package controller

import (
	"context"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/labels"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/utils/ptr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func TestReadersBuildAReadOnlyWorkloadBesideTheWriter(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 3
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})
	if got := ptr.Deref(deployment.Spec.Replicas, 0); got != 3 {
		t.Fatalf("reader replicas = %d, want the requested three", got)
	}
	container := deployment.Spec.Template.Spec.Containers[0]
	if got := environmentMap(container.Env)["RAD_ROLE"]; got != "read" {
		t.Fatalf("reader RAD_ROLE = %q", got)
	}
	if container.Image != "rad:test" {
		t.Fatalf("reader image = %q, want the writer's image", container.Image)
	}
	// A reader answers the same three questions as a writer. Losing a probe
	// here would leave a reader in a Service while it cannot serve.
	if container.StartupProbe == nil || container.ReadinessProbe == nil || container.LivenessProbe == nil {
		t.Fatal("reader container does not have startup, readiness, and liveness probes")
	}
	for _, key := range []string{"AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"} {
		variable := findEnvironmentVariable(t, container.Env, key)
		if variable.ValueFrom == nil || variable.ValueFrom.SecretKeyRef == nil || variable.ValueFrom.SecretKeyRef.Name != "alpha-s3" {
			t.Fatalf("reader %s projection = %#v", key, variable)
		}
	}
	// Surge before terminate: a rollout must not remove read capacity it has
	// not yet replaced.
	rollout := deployment.Spec.Strategy.RollingUpdate
	if deployment.Spec.Strategy.Type != appsv1.RollingUpdateDeploymentStrategyType || rollout == nil {
		t.Fatalf("reader strategy = %#v", deployment.Spec.Strategy)
	}
	if rollout.MaxUnavailable.IntValue() != 0 || rollout.MaxSurge.IntValue() != 1 {
		t.Fatalf("reader rolling update = %#v", rollout)
	}

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	// The rollout trigger must reach both workloads, or rotating a credential
	// would replace the writer and leave readers on the withdrawn one.
	writerAnnotations := statefulSet.Spec.Template.Annotations
	if got := deployment.Spec.Template.Annotations[credentialsVersionKey]; got == "" || got != writerAnnotations[credentialsVersionKey] {
		t.Fatalf("reader credential annotation = %q, writer = %q", got, writerAnnotations[credentialsVersionKey])
	}

	readerService := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &corev1.Service{})
	if readerService.Spec.Ports[0].Port != 80 || readerService.Spec.Ports[0].TargetPort != intstrFromInt(publicPort) {
		t.Fatalf("reader service ports = %#v", readerService.Spec.Ports)
	}
	budget := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &policyv1.PodDisruptionBudget{})
	if budget.Spec.MaxUnavailable == nil || budget.Spec.MaxUnavailable.IntValue() != 1 {
		t.Fatalf("reader disruption budget = %#v", budget.Spec)
	}

	// The published route reaches the writer. A reader cannot serve a write,
	// and a client that reads after writing must not be answered by a lagging
	// instance unless it asked for one.
	ingress := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.Ingress{})
	if got := ingress.Spec.Rules[0].HTTP.Paths[0].Backend.Service.Name; got != "rad-alpha" {
		t.Fatalf("ingress backend = %q, want the writer service", got)
	}
}

// Two workloads share the database labels. If either selector matched the
// other's pods, each workload's controller would act on pods it does not own.
func TestWriterAndReaderSelectorsAreDisjoint(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})

	writerPods := labels.Set(statefulSet.Spec.Template.Labels)
	readerPods := labels.Set(deployment.Spec.Template.Labels)
	for _, pair := range []struct {
		name     string
		selector *metav1.LabelSelector
		matches  labels.Set
		rejects  labels.Set
	}{
		{"writer", statefulSet.Spec.Selector, writerPods, readerPods},
		{"reader", deployment.Spec.Selector, readerPods, writerPods},
	} {
		selector, err := metav1.LabelSelectorAsSelector(pair.selector)
		if err != nil {
			t.Fatal(err)
		}
		if !selector.Matches(pair.matches) {
			t.Fatalf("%s selector %s does not match its own pods %s", pair.name, selector, pair.matches)
		}
		if selector.Matches(pair.rejects) {
			t.Fatalf("%s selector %s also matches the other role's pods %s", pair.name, selector, pair.rejects)
		}
	}

	// Every Service and budget must name a role for the same reason.
	for _, expectation := range []struct {
		name     string
		selector map[string]string
		role     string
	}{
		{"rad-alpha", nil, writeRole},
		{"rad-alpha-headless", nil, writeRole},
		{"rad-alpha-reader", nil, readRole},
	} {
		service := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: expectation.name}, &corev1.Service{})
		if got := service.Spec.Selector[roleLabel]; got != expectation.role {
			t.Fatalf("service %s selects role %q, want %q", expectation.name, got, expectation.role)
		}
	}
	writerBudget := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &policyv1.PodDisruptionBudget{})
	if got := writerBudget.Spec.Selector.MatchLabels[roleLabel]; got != writeRole {
		t.Fatalf("writer disruption budget selects role %q", got)
	}

	// The network policy deliberately covers both roles: every Rad pod in the
	// database accepts gateway traffic on the public port.
	policy := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.NetworkPolicy{})
	if _, scoped := policy.Spec.PodSelector.MatchLabels[roleLabel]; scoped {
		t.Fatalf("network policy is scoped to one role: %#v", policy.Spec.PodSelector)
	}
}

func TestNoReadersMeansNoReaderResources(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	assertReaderResourcesAbsent(t, kubernetesClient)
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	if observed.Status.ReaderServiceName != "" || observed.Status.DesiredReaders != 0 {
		t.Fatalf("status advertises readers that were never requested: %#v", observed.Status)
	}
}

// The API server rejects a negative count, so this only matters for a
// Database built in Go. Passing it through would ask for negative replicas.
func TestNegativeReaderCountBuildsNoReaders(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = -1
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	assertReaderResourcesAbsent(t, kubernetesClient)
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	if observed.Status.DesiredReaders != 0 {
		t.Fatalf("desiredReaders = %d, want none", observed.Status.DesiredReaders)
	}
}

func TestScalingReadersToZeroRemovesTheirResources(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})

	scaled := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	scaled.Spec.Readers = 0
	if err := kubernetesClient.Update(context.Background(), scaled); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}

	assertReaderResourcesAbsent(t, kubernetesClient)
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	if observed.Status.ReaderServiceName != "" || observed.Status.ReadyReaders != 0 {
		t.Fatalf("status still advertises readers: %#v", observed.Status)
	}
}

// A database whose writer and route are healthy is available. Readers add
// capacity, so a reader that is not yet ready must not withdraw the database.
func TestReaderReadinessDoesNotGateTheDatabase(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
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
	if condition == nil || condition.Status != metav1.ConditionTrue {
		t.Fatalf("no ready reader withdrew the whole database: %#v", condition)
	}
	if observed.Status.DesiredReaders != 2 || observed.Status.ReadyReaders != 0 {
		t.Fatalf("reader counts = %#v", observed.Status)
	}
	if observed.Status.ReaderServiceName != "rad-alpha-reader" {
		t.Fatalf("reader service name = %q", observed.Status.ReaderServiceName)
	}

	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})
	deployment.Status.ReadyReplicas = 2
	if err := kubernetesClient.Status().Update(context.Background(), deployment); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}
	observed = getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	if observed.Status.ReadyReaders != 2 {
		t.Fatalf("ready readers = %d, want two", observed.Status.ReadyReaders)
	}
}

// A rejected database must stop serving reads as well as writes.
func TestQuiescingStopsReadersWithTheWriter(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	if err := reconciler.quiesceRejectedDatabase(context.Background(), database); err != nil {
		t.Fatal(err)
	}
	for _, object := range []client.Object{
		&appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: "rad-alpha", Namespace: testNamespace}},
		&appsv1.Deployment{ObjectMeta: metav1.ObjectMeta{Name: "rad-alpha-reader", Namespace: testNamespace}},
	} {
		if err := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(object), object); !apierrors.IsNotFound(err) {
			t.Fatalf("%T still serves a rejected database: %v", object, err)
		}
	}
}

func TestReaderResourceNamesRemainDNSLabels(t *testing.T) {
	name := "tenant-with-a-name-that-is-deliberately-longer-than-a-derived-resource-allows"
	first := readerResourceName(name)
	if len(first) > 63 || first != readerResourceName(name) {
		t.Fatalf("reader resource name = %q", first)
	}
	if first == headlessServiceName(name) {
		t.Fatalf("truncated reader and headless names collide at %q", first)
	}
	if got := readerResourceName("alpha"); got != "rad-alpha-reader" {
		t.Fatalf("reader resource name = %q", got)
	}
}

func assertReaderResourcesAbsent(t *testing.T, kubernetesClient client.Client) {
	t.Helper()
	key := types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}
	for _, object := range []client.Object{
		&appsv1.Deployment{},
		&corev1.Service{},
		&policyv1.PodDisruptionBudget{},
	} {
		if err := kubernetesClient.Get(context.Background(), key, object); !apierrors.IsNotFound(err) {
			t.Fatalf("%T exists without readers: %v", object, err)
		}
	}
}
