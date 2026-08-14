//go:build envtest

package controller

import (
	"context"
	"errors"
	"sync"
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
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/cache"
	"sigs.k8s.io/controller-runtime/pkg/client"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

// The reconciliation state machine against a real API server: idempotent
// convergence, drift repair, teardown ordering, restart recovery, and the
// manager's watch wiring. Anything needing kubelet behaviour (probes, pods,
// fencing) belongs to the kind e2e suite.
//
// Two envtest realities shape these tests. Reconciliation installs the
// finalizer on its first pass and provisions on later passes, so provisioning
// is always driven to a fixpoint rather than counted in passes. And envtest
// runs no garbage collector, so the operator's foreground child deletion
// never completes on its own — [foregroundGCStandIn] substitutes exactly that
// one missing behaviour by stripping the foregroundDeletion finalizer, while
// real GC-backed teardown is proven by the kind e2e suite.

func envtestReconciler(kubernetesClient client.Client, scheme *runtime.Scheme, namespace string) *DatabaseReconciler {
	return &DatabaseReconciler{
		Client:                  kubernetesClient,
		APIReader:               kubernetesClient,
		Scheme:                  scheme,
		RadImage:                "rad:test",
		IngressClass:            "test",
		GatewayNamespace:        namespace,
		RequeueInterval:         time.Millisecond,
		DependencyPollInterval:  time.Millisecond,
		CredentialPollInterval:  time.Millisecond,
		MaxConcurrentReconciles: 2,
	}
}

func envtestSecret(t *testing.T, kubernetesClient client.Client, namespace, name string) {
	t.Helper()
	secret := testSecret(name)
	secret.Namespace = namespace
	secret.ResourceVersion = ""
	secret.UID = ""
	if err := kubernetesClient.Create(context.Background(), secret); err != nil {
		t.Fatal(err)
	}
}

func reconcileOnce(t *testing.T, reconciler *DatabaseReconciler, namespace, name string) {
	t.Helper()
	request := ctrl.Request{NamespacedName: types.NamespacedName{Namespace: namespace, Name: name}}
	if _, err := reconciler.Reconcile(context.Background(), request); err != nil {
		t.Fatal(err)
	}
}

// provision reconciles until the tenant StatefulSet exists.
func provision(t *testing.T, reconciler *DatabaseReconciler, kubernetesClient client.Client, namespace, name string) {
	t.Helper()
	key := types.NamespacedName{Namespace: namespace, Name: resourceName(name)}
	for range 5 {
		reconcileOnce(t, reconciler, namespace, name)
		if err := kubernetesClient.Get(context.Background(), key, &appsv1.StatefulSet{}); err == nil {
			return
		}
	}
	t.Fatalf("database %s never provisioned", name)
}

// foregroundGCStandIn clears foregroundDeletion finalizers in one namespace
// until the returned stop function runs, standing in for the garbage
// collector envtest does not have. Every kind the operator foreground-deletes
// is covered, including its claim Leases.
func foregroundGCStandIn(t *testing.T, kubernetesClient client.Client, namespace string) func() {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			select {
			case <-ctx.Done():
				return
			case <-time.After(50 * time.Millisecond):
			}
			for _, list := range []client.ObjectList{
				&networkingv1.IngressList{},
				&appsv1.StatefulSetList{},
				&corev1.ServiceList{},
				&networkingv1.NetworkPolicyList{},
				&policyv1.PodDisruptionBudgetList{},
				&corev1.ServiceAccountList{},
				&coordinationv1.LeaseList{},
			} {
				if kubernetesClient.List(ctx, list, client.InNamespace(namespace)) != nil {
					continue
				}
				items, err := meta.ExtractList(list)
				if err != nil {
					continue
				}
				for _, item := range items {
					stripForegroundFinalizer(ctx, kubernetesClient, item.(client.Object))
				}
			}
		}
	}()
	return func() {
		cancel()
		<-done
	}
}

func stripForegroundFinalizer(ctx context.Context, kubernetesClient client.Client, object client.Object) {
	if object.GetDeletionTimestamp().IsZero() {
		return
	}
	kept := make([]string, 0, len(object.GetFinalizers()))
	for _, finalizer := range object.GetFinalizers() {
		if finalizer != "foregroundDeletion" {
			kept = append(kept, finalizer)
		}
	}
	if len(kept) == len(object.GetFinalizers()) {
		return
	}
	object.SetFinalizers(kept)
	_ = kubernetesClient.Update(ctx, object)
}

func childObjects(name string) map[string]client.Object {
	return map[string]client.Object{
		"statefulset":   &appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: resourceName(name)}},
		"service":       &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: resourceName(name)}},
		"headless":      &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: headlessServiceName(name)}},
		"ingress":       &networkingv1.Ingress{ObjectMeta: metav1.ObjectMeta{Name: resourceName(name)}},
		"networkpolicy": &networkingv1.NetworkPolicy{ObjectMeta: metav1.ObjectMeta{Name: resourceName(name)}},
		"pdb":           &policyv1.PodDisruptionBudget{ObjectMeta: metav1.ObjectMeta{Name: resourceName(name)}},
	}
}

func childVersions(t *testing.T, kubernetesClient client.Client, namespace, name string) map[string]string {
	t.Helper()
	versions := map[string]string{}
	for kind, object := range childObjects(name) {
		key := types.NamespacedName{Namespace: namespace, Name: object.GetName()}
		if err := kubernetesClient.Get(context.Background(), key, object); err != nil {
			t.Fatalf("get %s: %v", kind, err)
		}
		versions[kind] = object.GetResourceVersion()
	}
	return versions
}

// Reconciling a converged database must write nothing: spurious child updates
// are not cosmetic, they roll the single writer.
func TestRepeatedReconcileConvergesWithoutRewrites(t *testing.T) {
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "converge")
	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")
	database := envtestDatabase("tenant", namespace, "converge-bucket", "converge.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(context.Background(), database); err != nil {
		t.Fatal(err)
	}

	reconciler := envtestReconciler(kubernetesClient, scheme, namespace)
	provision(t, reconciler, kubernetesClient, namespace, "tenant")
	reconcileOnce(t, reconciler, namespace, "tenant")
	settled := childVersions(t, kubernetesClient, namespace, "tenant")

	for range 5 {
		reconcileOnce(t, reconciler, namespace, "tenant")
	}
	for kind, version := range childVersions(t, kubernetesClient, namespace, "tenant") {
		if version != settled[kind] {
			t.Errorf("%s was rewritten by a converged reconcile: %s -> %s", kind, settled[kind], version)
		}
	}
}

func TestReconcileRepairsDeletedAndMutatedChildren(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "drift")
	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")
	database := envtestDatabase("tenant", namespace, "drift-bucket", "drift.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}
	reconciler := envtestReconciler(kubernetesClient, scheme, namespace)
	provision(t, reconciler, kubernetesClient, namespace, "tenant")

	// Deleting through the API (not the operator) has no foreground policy,
	// so no GC stand-in is needed for the drift itself.
	statefulSet := &appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: "rad-tenant", Namespace: namespace}}
	if err := kubernetesClient.Delete(ctx, statefulSet); err != nil {
		t.Fatal(err)
	}
	service := &corev1.Service{}
	if err := kubernetesClient.Get(ctx, types.NamespacedName{Namespace: namespace, Name: "rad-tenant"}, service); err != nil {
		t.Fatal(err)
	}
	service.Spec.Ports[0].Port = 81
	if err := kubernetesClient.Update(ctx, service); err != nil {
		t.Fatal(err)
	}

	reconcileOnce(t, reconciler, namespace, "tenant")
	if err := kubernetesClient.Get(ctx, types.NamespacedName{Namespace: namespace, Name: "rad-tenant"}, statefulSet); err != nil {
		t.Fatalf("deleted StatefulSet was not recreated: %v", err)
	}
	if err := kubernetesClient.Get(ctx, types.NamespacedName{Namespace: namespace, Name: "rad-tenant"}, service); err != nil {
		t.Fatal(err)
	}
	if service.Spec.Ports[0].Port != 80 {
		t.Fatalf("mutated Service port = %d, want repaired to 80", service.Spec.Ports[0].Port)
	}
}

func TestSpecChangeReconcilesAndTracksGeneration(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "spec")
	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")
	database := envtestDatabase("tenant", namespace, "spec-bucket", "spec.rad.example", "tenant-s3")
	database.Spec.Route.Scheme = "https"
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}
	reconciler := envtestReconciler(kubernetesClient, scheme, namespace)
	provision(t, reconciler, kubernetesClient, namespace, "tenant")

	observed := getEnvtestDatabase(t, kubernetesClient, database)
	if observed.Status.ObservedGeneration != observed.Generation {
		t.Fatalf("observedGeneration = %d, generation = %d", observed.Status.ObservedGeneration, observed.Generation)
	}
	if observed.Status.URL != "https://spec.rad.example" {
		t.Fatalf("status URL = %q", observed.Status.URL)
	}

	observed.Spec.Route.Scheme = "http"
	if err := kubernetesClient.Update(ctx, observed); err != nil {
		t.Fatal(err)
	}
	reconcileOnce(t, reconciler, namespace, "tenant")
	observed = getEnvtestDatabase(t, kubernetesClient, database)
	if observed.Status.URL != "http://spec.rad.example" {
		t.Fatalf("status URL after scheme change = %q", observed.Status.URL)
	}
	if observed.Status.ObservedGeneration != observed.Generation || observed.Generation < 2 {
		t.Fatalf("observedGeneration = %d, generation = %d after update", observed.Status.ObservedGeneration, observed.Generation)
	}
}

func reconcileUntilGone(t *testing.T, reconciler *DatabaseReconciler, kubernetesClient client.Client, namespace, name string) {
	t.Helper()
	deadline := time.Now().Add(30 * time.Second)
	for {
		reconcileOnce(t, reconciler, namespace, name)
		current := &radv1alpha1.Database{}
		err := kubernetesClient.Get(context.Background(),
			types.NamespacedName{Namespace: namespace, Name: name}, current)
		if apierrors.IsNotFound(err) {
			return
		}
		if err != nil {
			t.Fatal(err)
		}
		if time.Now().After(deadline) {
			var leases coordinationv1.LeaseList
			_ = kubernetesClient.List(context.Background(), &leases, client.InNamespace(namespace))
			remaining := ""
			for kind, object := range childObjects(name) {
				key := types.NamespacedName{Namespace: namespace, Name: object.GetName()}
				if err := kubernetesClient.Get(context.Background(), key, object); err == nil {
					remaining += kind + "(finalizers=" + joinStrings(object.GetFinalizers()) + ") "
				}
			}
			serviceAccount := &corev1.ServiceAccount{}
			if err := kubernetesClient.Get(context.Background(),
				types.NamespacedName{Namespace: namespace, Name: resourceName(name)}, serviceAccount); err == nil {
				remaining += "serviceaccount(finalizers=" + joinStrings(serviceAccount.GetFinalizers()) + ") "
			}
			t.Fatalf("database %s never finished deleting: finalizers=%v leases=%d remaining=%s",
				name, current.Finalizers, len(leases.Items), remaining)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

func TestDeleteTearsDownEveryChildAndReleasesClaims(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "teardown")
	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")
	database := envtestDatabase("tenant", namespace, "teardown-bucket", "teardown.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}
	reconciler := envtestReconciler(kubernetesClient, scheme, namespace)
	provision(t, reconciler, kubernetesClient, namespace, "tenant")

	stopGC := foregroundGCStandIn(t, kubernetesClient, namespace)
	defer stopGC()
	if err := kubernetesClient.Delete(ctx, getEnvtestDatabase(t, kubernetesClient, database)); err != nil {
		t.Fatal(err)
	}
	reconcileUntilGone(t, reconciler, kubernetesClient, namespace, "tenant")

	for kind, object := range childObjects("tenant") {
		key := types.NamespacedName{Namespace: namespace, Name: object.GetName()}
		if err := kubernetesClient.Get(ctx, key, object); !apierrors.IsNotFound(err) {
			t.Errorf("%s survived deletion: %v", kind, err)
		}
	}
	var leases coordinationv1.LeaseList
	if err := kubernetesClient.List(ctx, &leases, client.InNamespace(namespace)); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) != 0 {
		t.Fatalf("%d claim Leases survived deletion", len(leases.Items))
	}
}

// Deleting a database that never provisioned (its Secret is absent) must not
// wedge on cleanup that has nothing to clean.
func TestDeleteWhileProvisioningIsBlockedStillCompletes(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "blocked")
	database := envtestDatabase("tenant", namespace, "blocked-bucket", "blocked.rad.example", "absent-secret")
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}
	reconciler := envtestReconciler(kubernetesClient, scheme, namespace)
	// Pass one installs the finalizer; pass two acquires claims and blocks on
	// the missing Secret.
	reconcileOnce(t, reconciler, namespace, "tenant")
	reconcileOnce(t, reconciler, namespace, "tenant")

	observed := getEnvtestDatabase(t, kubernetesClient, database)
	credentials := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionCredentialsReady)
	if credentials == nil || credentials.Status != metav1.ConditionFalse {
		t.Fatalf("CredentialsReady = %#v, want False while the Secret is absent", credentials)
	}

	stopGC := foregroundGCStandIn(t, kubernetesClient, namespace)
	defer stopGC()
	if err := kubernetesClient.Delete(ctx, observed); err != nil {
		t.Fatal(err)
	}
	reconcileUntilGone(t, reconciler, kubernetesClient, namespace, "tenant")
}

// A controller restart between the claim phase and the workload phase must
// resume exactly where the previous instance stopped, without duplicating
// claims.
func TestControllerRestartResumesProvisioning(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "restart")
	database := envtestDatabase("tenant", namespace, "restart-bucket", "restart.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}

	first := envtestReconciler(kubernetesClient, scheme, namespace)
	reconcileOnce(t, first, namespace, "tenant")
	reconcileOnce(t, first, namespace, "tenant")
	var leases coordinationv1.LeaseList
	if err := kubernetesClient.List(ctx, &leases, client.InNamespace(namespace)); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) == 0 {
		t.Fatal("no claims were acquired before the simulated restart")
	}

	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")
	second := envtestReconciler(kubernetesClient, scheme, namespace)
	provision(t, second, kubernetesClient, namespace, "tenant")

	if err := kubernetesClient.List(ctx, &leases, client.InNamespace(namespace)); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) != 2 {
		t.Fatalf("claim Leases after restart = %d, want exactly storage and route", len(leases.Items))
	}
}

func TestConcurrentReconcilesOfOneDatabaseConverge(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "race")
	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")
	database := envtestDatabase("tenant", namespace, "race-bucket", "race.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}

	reconciler := envtestReconciler(kubernetesClient, scheme, namespace)
	request := ctrl.Request{NamespacedName: types.NamespacedName{Namespace: namespace, Name: "tenant"}}
	failures := make(chan error, 8)
	var workers sync.WaitGroup
	for range 4 {
		workers.Add(1)
		go func() {
			defer workers.Done()
			for range 2 {
				if _, err := reconciler.Reconcile(ctx, request); err != nil {
					failures <- err
				}
			}
		}()
	}
	workers.Wait()
	close(failures)
	for err := range failures {
		// controller-runtime never runs one key concurrently, so this test is
		// stricter than production. Racing passes may see optimistic-conflict
		// and already-exists errors that the next pass resolves; anything else
		// is a real failure.
		if !apierrors.IsConflict(err) && !apierrors.IsAlreadyExists(err) && !errors.Is(err, context.Canceled) {
			t.Fatalf("racing reconcile failed: %v", err)
		}
	}

	provision(t, reconciler, kubernetesClient, namespace, "tenant")
	var leases coordinationv1.LeaseList
	if err := kubernetesClient.List(ctx, &leases, client.InNamespace(namespace)); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) != 2 {
		t.Fatalf("claim Leases after racing reconciles = %d, want 2", len(leases.Items))
	}
}

// The manager path is what production runs: watches, cache, and the claim
// field index drive reconciliation with no manual Reconcile calls at all.
func TestManagerWatchesDriveTheFullLifecycle(t *testing.T) {
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "manager")
	envtestSecret(t, kubernetesClient, namespace, "tenant-s3")

	manager, err := ctrl.NewManager(envtestConfig, ctrl.Options{
		Scheme:                 scheme,
		Metrics:                metricsserver.Options{BindAddress: "0"},
		HealthProbeBindAddress: "0",
		Cache:                  cache.Options{DefaultNamespaces: map[string]cache.Config{namespace: {}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	reconciler := &DatabaseReconciler{
		Client:                  manager.GetClient(),
		APIReader:               manager.GetAPIReader(),
		Scheme:                  scheme,
		Recorder:                manager.GetEventRecorderFor("rad-operator-envtest"),
		RadImage:                "rad:test",
		IngressClass:            "test",
		GatewayNamespace:        namespace,
		RequeueInterval:         50 * time.Millisecond,
		DependencyPollInterval:  50 * time.Millisecond,
		CredentialPollInterval:  50 * time.Millisecond,
		MaxConcurrentReconciles: 2,
	}
	if err := reconciler.SetupWithManager(manager); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	managerStopped := make(chan error, 1)
	go func() { managerStopped <- manager.Start(ctx) }()

	database := envtestDatabase("tenant", namespace, "manager-bucket", "manager.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(context.Background(), database); err != nil {
		t.Fatal(err)
	}
	statefulSetKey := types.NamespacedName{Namespace: namespace, Name: "rad-tenant"}
	waitForCondition(t, 30*time.Second, "watch-driven provisioning", func() bool {
		return kubernetesClient.Get(context.Background(), statefulSetKey, &appsv1.StatefulSet{}) == nil
	})

	// Deleting a child must be repaired through the Owns watch alone.
	if err := kubernetesClient.Delete(context.Background(),
		&appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: "rad-tenant", Namespace: namespace}}); err != nil {
		t.Fatal(err)
	}
	waitForCondition(t, 30*time.Second, "watch-driven drift repair", func() bool {
		current := &appsv1.StatefulSet{}
		return kubernetesClient.Get(context.Background(), statefulSetKey, current) == nil &&
			current.DeletionTimestamp.IsZero()
	})

	stopGC := foregroundGCStandIn(t, kubernetesClient, namespace)
	defer stopGC()
	if err := kubernetesClient.Delete(context.Background(), getEnvtestDatabase(t, kubernetesClient, database)); err != nil {
		t.Fatal(err)
	}
	waitForCondition(t, 30*time.Second, "watch-driven teardown", func() bool {
		err := kubernetesClient.Get(context.Background(),
			types.NamespacedName{Namespace: namespace, Name: "tenant"}, &radv1alpha1.Database{})
		return apierrors.IsNotFound(err)
	})

	cancel()
	if err := <-managerStopped; err != nil {
		t.Fatalf("manager stopped with error: %v", err)
	}
}

func joinStrings(values []string) string {
	joined := ""
	for _, value := range values {
		joined += value + ","
	}
	return joined
}

func waitForCondition(t *testing.T, timeout time.Duration, what string, poll func() bool) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for {
		if poll() {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s", what)
		}
		time.Sleep(100 * time.Millisecond)
	}
}
