//go:build envtest

package cluster

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/client-go/kubernetes"
	"k8s.io/client-go/rest"
	"sigs.k8s.io/controller-runtime/pkg/envtest"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

var envtestConfig *rest.Config

func TestMain(main *testing.M) {
	environment := &envtest.Environment{
		CRDDirectoryPaths:        []string{filepath.Join("..", "config", "install", "crd")},
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
		forcedCleanup := strings.Contains(err.Error(), "timeout waiting for process kube-apiserver to stop")
		fmt.Fprintf(os.Stderr, "stop envtest: %v (forced cleanup: %t)\n", err, forcedCleanup)
		if !forcedCleanup && code == 0 {
			code = 1
		}
	}
	os.Exit(code)
}

// The SDK against a real API server: discovery, server-side defaulting, CEL
// validation surfaced through SDK errors, and the sentinel error contract.
func TestSDKAgainstARealAPIServer(t *testing.T) {
	ctx := context.Background()
	sdk, err := New(WithRESTConfig(envtestConfig), WithNamespace("default"))
	if err != nil {
		t.Fatal(err)
	}

	created, err := sdk.CreateDatabase(ctx, DatabaseSpec{
		Name:              "tenant",
		Bucket:            "tenant-bucket",
		CredentialsSecret: "tenant-s3",
		Hostname:          "tenant.rad.example",
	})
	if err != nil {
		t.Fatal(err)
	}
	if created.Ready {
		t.Fatal("create reported ready with no operator running")
	}

	// Server-side defaulting is visible on the stored resource even though the
	// SDK spec never set those fields.
	raw := &radv1alpha1.Database{}
	if err := sdk.kube.Get(ctx, types.NamespacedName{Namespace: "default", Name: "tenant"}, raw); err != nil {
		t.Fatal(err)
	}
	if raw.Spec.Storage.Prefix != "rad" || raw.Spec.Storage.Region != "us-east-1" ||
		raw.Spec.Route.Scheme != "https" || raw.Spec.CatalogMode != radv1alpha1.CatalogModeSchema {
		t.Fatalf("server defaults = %#v", raw.Spec)
	}

	if _, err := sdk.CreateDatabase(ctx, DatabaseSpec{
		Name: "tenant", Bucket: "other", CredentialsSecret: "s", Hostname: "other.rad.example",
	}); !errors.Is(err, ErrAlreadyExists) {
		t.Fatalf("duplicate create error = %v, want ErrAlreadyExists", err)
	}

	// CEL validation the SDK does not duplicate client-side surfaces as a
	// create error: a TLS Secret requires the https scheme.
	if _, err := sdk.CreateDatabase(ctx, DatabaseSpec{
		Name:              "invalid-tls",
		Bucket:            "invalid-tls-bucket",
		CredentialsSecret: "s",
		Hostname:          "invalid-tls.rad.example",
		Scheme:            "http",
		TLSSecret:         "some-tls",
	}); err == nil || !strings.Contains(err.Error(), "https") {
		t.Fatalf("invalid TLS spec error = %v, want the server's CEL message", err)
	}

	listed, err := sdk.ListDatabases(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 1 || listed[0].Name != "tenant" {
		t.Fatalf("list = %+v", listed)
	}

	if err := sdk.DeleteDatabase(ctx, "tenant"); err != nil {
		t.Fatal(err)
	}
	if _, err := sdk.GetDatabase(ctx, "tenant"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("get after delete error = %v, want ErrNotFound", err)
	}
	if err := sdk.DeleteDatabase(ctx, "tenant"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("repeated delete error = %v, want ErrNotFound", err)
	}
}

func TestWaitReadyReportsTheBlockingConditionOverARealAPIServer(t *testing.T) {
	ctx := context.Background()
	sdk, err := New(WithRESTConfig(envtestConfig), WithNamespace("default"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := sdk.CreateDatabase(ctx, DatabaseSpec{
		Name:              "waiting",
		Bucket:            "waiting-bucket",
		CredentialsSecret: "waiting-s3",
		Hostname:          "waiting.rad.example",
	}); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = sdk.DeleteDatabase(context.Background(), "waiting") })

	// With no operator, status stays unobserved; a bounded wait must say so.
	bounded, cancel := context.WithTimeout(ctx, 500*time.Millisecond)
	defer cancel()
	_, err = sdk.WaitReady(bounded, "waiting")
	if err == nil || !strings.Contains(err.Error(), "Pending") {
		t.Fatalf("wait error = %v, want the pending state named", err)
	}

	// Once a controller reports a blocking dependency, the timeout names it.
	raw := &radv1alpha1.Database{}
	if err := sdk.kube.Get(ctx, types.NamespacedName{Namespace: "default", Name: "waiting"}, raw); err != nil {
		t.Fatal(err)
	}
	raw.Status.Conditions = []metav1.Condition{{
		Type: radv1alpha1.ConditionReady, Status: metav1.ConditionFalse,
		Reason: "BucketClaimConflict", Message: "storage claim is held by another database",
		LastTransitionTime: metav1.Now(), ObservedGeneration: raw.Generation,
	}}
	if err := sdk.kube.Status().Update(ctx, raw); err != nil {
		t.Fatal(err)
	}
	bounded, cancel = context.WithTimeout(ctx, 500*time.Millisecond)
	defer cancel()
	_, err = sdk.WaitReady(bounded, "waiting")
	if err == nil || !strings.Contains(err.Error(), "BucketClaimConflict") {
		t.Fatalf("wait error = %v, want the blocking claim named", err)
	}
}

// A cluster without the Rad CRD must be reported as not installed, which the
// live e2e suite can never test because its cluster always has the operator.
func TestBareClusterReportsNotInstalled(t *testing.T) {
	bare := &envtest.Environment{
		ControlPlaneStartTimeout: time.Minute,
		ControlPlaneStopTimeout:  5 * time.Second,
	}
	configuration, err := bare.Start()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = bare.Stop() })

	if _, err := New(WithRESTConfig(configuration)); !errors.Is(err, ErrNotInstalled) {
		t.Fatalf("New against a bare cluster = %v, want ErrNotInstalled", err)
	}
}

// Guard against the namespace option silently applying to the wrong scope.
func TestNamespaceOptionScopesEveryOperation(t *testing.T) {
	ctx := context.Background()
	clientset, err := kubernetes.NewForConfig(envtestConfig)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := clientset.CoreV1().Namespaces().Create(ctx,
		&corev1.Namespace{ObjectMeta: metav1.ObjectMeta{Name: "elsewhere"}},
		metav1.CreateOptions{}); err != nil {
		t.Fatal(err)
	}

	other, err := New(WithRESTConfig(envtestConfig), WithNamespace("elsewhere"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := other.CreateDatabase(ctx, DatabaseSpec{
		Name: "scoped", Bucket: "scoped-bucket", CredentialsSecret: "s", Hostname: "scoped.rad.example",
	}); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = other.DeleteDatabase(context.Background(), "scoped") })

	sdk, err := New(WithRESTConfig(envtestConfig), WithNamespace("default"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := sdk.GetDatabase(ctx, "scoped"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("cross-namespace get = %v, want ErrNotFound", err)
	}
}
