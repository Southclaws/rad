//go:build envtest

package controller

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"k8s.io/client-go/rest"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/envtest"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

var envtestConfig *rest.Config

func TestMain(main *testing.M) {
	environment := &envtest.Environment{
		CRDDirectoryPaths:        []string{filepath.Join("..", "..", "config", "install", "crd")},
		ErrorIfCRDPathMissing:    true,
		ControlPlaneStartTimeout: time.Minute,
		ControlPlaneStopTimeout:  5 * time.Second,
	}
	configuration, err := environment.Start()
	if err != nil {
		fmt.Fprintf(os.Stderr, "start envtest: %v\n", err)
		os.Exit(1)
	}
	envtestConfig = configuration
	code := main.Run()
	if err := environment.Stop(); err != nil {
		// envtest sends SIGKILL before returning this exact timeout, so the
		// temporary API server is still cleaned up on desktop runtimes where
		// the test binary does not exit after SIGTERM.
		forcedCleanup := strings.Contains(err.Error(), "timeout waiting for process kube-apiserver to stop")
		fmt.Fprintf(os.Stderr, "stop envtest: %v (forced cleanup: %t)\n", err, forcedCleanup)
		if !forcedCleanup && code == 0 {
			code = 1
		}
	}
	os.Exit(code)
}

func TestAPIServerDefaultsAndPermitsAuthenticationRotation(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, _ := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "api")
	secret := testSecret("tenant-s3")
	secret.Namespace = namespace
	secret.ResourceVersion = ""
	secret.UID = ""
	if err := kubernetesClient.Create(ctx, secret); err != nil {
		t.Fatal(err)
	}
	database := envtestDatabase("tenant", namespace, "shared-bucket", "tenant.rad.example", "tenant-s3")
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}

	observed := getEnvtestDatabase(t, kubernetesClient, database)
	if observed.Spec.Storage.Prefix != "rad" || observed.Spec.Storage.Region != "us-east-1" || observed.Spec.CatalogMode != radv1alpha1.CatalogModeSchema {
		t.Fatalf("API defaults = %#v", observed.Spec)
	}
	if observed.Spec.Route.Scheme != "https" || observed.Spec.DeletionPolicy != radv1alpha1.DeletionPolicyRetain || observed.Spec.TerminationGracePeriodSeconds != 120 {
		t.Fatalf("route/lifecycle defaults = %#v", observed.Spec)
	}

	serviceAccount := &corev1.ServiceAccount{ObjectMeta: metav1.ObjectMeta{Name: "tenant-workload", Namespace: namespace}}
	if err := kubernetesClient.Create(ctx, serviceAccount); err != nil {
		t.Fatal(err)
	}
	observed.Spec.Storage.Authentication = radv1alpha1.S3Authentication{ServiceAccountName: &serviceAccount.Name}
	if err := kubernetesClient.Update(ctx, observed); err != nil {
		t.Fatalf("authentication rotation rejected: %v", err)
	}

	observed = getEnvtestDatabase(t, kubernetesClient, database)
	observed.Spec.Storage.Bucket = "different-bucket"
	if err := kubernetesClient.Update(ctx, observed); !apierrors.IsInvalid(err) {
		t.Fatalf("immutable storage update error = %v, want Invalid", err)
	}

	invalidTLS := envtestDatabase("invalid-tls", namespace, "tls-bucket", "invalid-tls.rad.example", secret.Name)
	invalidTLS.Spec.Route.Scheme = "http"
	invalidTLS.Spec.Route.TLSSecretName = "tenant-tls"
	if err := kubernetesClient.Create(ctx, invalidTLS); !apierrors.IsInvalid(err) {
		t.Fatalf("HTTP route with TLS Secret error = %v, want Invalid", err)
	}

	missingAuthentication := envtestDatabase("missing-auth", namespace, "auth-bucket", "missing-auth.rad.example", secret.Name)
	missingAuthentication.Spec.Storage.Authentication = radv1alpha1.S3Authentication{}
	if err := kubernetesClient.Create(ctx, missingAuthentication); !apierrors.IsInvalid(err) {
		t.Fatalf("missing authentication error = %v, want Invalid", err)
	}
}

func TestAPIServerSerializesCompetingStorageClaims(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "claims")
	alpha := envtestDatabase("alpha", namespace, "shared-bucket", "alpha.rad.example", "alpha-s3")
	beta := envtestDatabase("beta", namespace, "shared-bucket", "beta.rad.example", "beta-s3")
	for _, database := range []*radv1alpha1.Database{alpha, beta} {
		if err := kubernetesClient.Create(ctx, database); err != nil {
			t.Fatal(err)
		}
	}
	alpha = getEnvtestDatabase(t, kubernetesClient, alpha)
	beta = getEnvtestDatabase(t, kubernetesClient, beta)
	reconciler := &DatabaseReconciler{Client: kubernetesClient, APIReader: kubernetesClient, Scheme: scheme}

	type result struct {
		problem *dependencyProblem
		err     error
	}
	results := make(chan result, 2)
	start := make(chan struct{})
	var workers sync.WaitGroup
	for _, database := range []*radv1alpha1.Database{alpha, beta} {
		workers.Add(1)
		go func(database *radv1alpha1.Database) {
			defer workers.Done()
			<-start
			problem, err := reconciler.acquireClaim(ctx, database, durableClaim{
				kind: storageClaimKind, key: storageClaim(database), conflictReason: "BucketClaimConflict",
			}, claimName(storageClaimKind, storageClaim(database)))
			results <- result{problem: problem, err: err}
		}(database)
	}
	close(start)
	workers.Wait()
	close(results)

	var acquired, conflicted int
	for result := range results {
		if result.err != nil {
			t.Fatal(result.err)
		}
		if result.problem == nil {
			acquired++
		} else if result.problem.reason == "BucketClaimConflict" {
			conflicted++
		}
	}
	if acquired != 1 || conflicted != 1 {
		t.Fatalf("claim outcomes: acquired=%d conflicted=%d", acquired, conflicted)
	}
	var leases coordinationv1.LeaseList
	if err := kubernetesClient.List(ctx, &leases, client.InNamespace(namespace)); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) != 1 {
		t.Fatalf("storage claim Leases = %d, want 1", len(leases.Items))
	}
}

func TestReconcileAgainstAPIServerBuildsTheTenantRuntime(t *testing.T) {
	ctx := context.Background()
	kubernetesClient, scheme := envtestClient(t)
	namespace := createEnvtestNamespace(t, kubernetesClient, "runtime")
	secret := testSecret("tenant-s3")
	secret.Namespace = namespace
	secret.ResourceVersion = ""
	secret.UID = ""
	if err := kubernetesClient.Create(ctx, secret); err != nil {
		t.Fatal(err)
	}
	database := envtestDatabase("tenant", namespace, "tenant-bucket", "tenant.rad.example", secret.Name)
	if err := kubernetesClient.Create(ctx, database); err != nil {
		t.Fatal(err)
	}
	reconciler := &DatabaseReconciler{
		Client:                  kubernetesClient,
		APIReader:               kubernetesClient,
		Scheme:                  scheme,
		RadImage:                "rad:test",
		IngressClass:            "test",
		GatewayNamespace:        namespace,
		DependencyPollInterval:  time.Millisecond,
		CredentialPollInterval:  time.Millisecond,
		MaxConcurrentReconciles: 2,
	}
	request := ctrl.Request{NamespacedName: types.NamespacedName{Namespace: namespace, Name: database.Name}}
	for range 2 {
		if _, err := reconciler.Reconcile(ctx, request); err != nil {
			t.Fatal(err)
		}
	}

	for _, object := range []client.Object{
		&appsv1.StatefulSet{},
		&corev1.Service{},
		&networkingv1.Ingress{},
		&networkingv1.NetworkPolicy{},
		&policyv1.PodDisruptionBudget{},
	} {
		if err := kubernetesClient.Get(ctx, types.NamespacedName{Namespace: namespace, Name: "rad-tenant"}, object); err != nil {
			t.Fatalf("get %T: %v", object, err)
		}
	}
	var leases coordinationv1.LeaseList
	if err := kubernetesClient.List(ctx, &leases, client.InNamespace(namespace)); err != nil {
		t.Fatal(err)
	}
	if len(leases.Items) != 2 {
		t.Fatalf("claim Leases = %d, want storage and route", len(leases.Items))
	}
}

func envtestClient(t *testing.T) (client.Client, *runtime.Scheme) {
	t.Helper()
	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := radv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	kubernetesClient, err := client.New(envtestConfig, client.Options{Scheme: scheme})
	if err != nil {
		t.Fatal(err)
	}
	return kubernetesClient, scheme
}

func createEnvtestNamespace(t *testing.T, kubernetesClient client.Client, suffix string) string {
	t.Helper()
	namespace := fmt.Sprintf("rad-%s-%d", suffix, time.Now().UnixNano())
	if err := kubernetesClient.Create(context.Background(), &corev1.Namespace{ObjectMeta: metav1.ObjectMeta{Name: namespace}}); err != nil {
		t.Fatal(err)
	}
	return namespace
}

func envtestDatabase(name, namespace, bucket, hostname, secretName string) *radv1alpha1.Database {
	return &radv1alpha1.Database{
		TypeMeta:   metav1.TypeMeta{APIVersion: radv1alpha1.GroupVersion.String(), Kind: "Database"},
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: namespace},
		Spec: radv1alpha1.DatabaseSpec{
			Storage: radv1alpha1.S3Storage{
				Bucket: bucket,
				Authentication: radv1alpha1.S3Authentication{
					CredentialsSecretRef: &corev1.LocalObjectReference{Name: secretName},
				},
			},
			Route: radv1alpha1.Route{Hostname: hostname},
		},
	}
}

func getEnvtestDatabase(t *testing.T, kubernetesClient client.Client, database *radv1alpha1.Database) *radv1alpha1.Database {
	t.Helper()
	observed := &radv1alpha1.Database{}
	if err := kubernetesClient.Get(context.Background(), client.ObjectKeyFromObject(database), observed); err != nil {
		t.Fatal(err)
	}
	return observed
}
