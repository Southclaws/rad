package cluster

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func testClient(t *testing.T) *Client {
	t.Helper()
	scheme := runtime.NewScheme()
	if err := radv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	kube := fake.NewClientBuilder().
		WithScheme(scheme).
		WithStatusSubresource(&radv1alpha1.Database{}).
		Build()
	return &Client{kube: kube, namespace: "tenants"}
}

func validSpec(name string) DatabaseSpec {
	return DatabaseSpec{
		Name:              name,
		Bucket:            name + "-bucket",
		CredentialsSecret: name + "-s3",
		Hostname:          name + ".rad.example.com",
	}
}

func TestCreateGetDeleteRoundTrip(t *testing.T) {
	c := testClient(t)
	created, err := c.CreateDatabase(context.Background(), validSpec("alpha"))
	if err != nil {
		t.Fatal(err)
	}
	if created.Ready || created.Reason != "Pending" {
		t.Fatalf("fresh database view = %+v, want unobserved and pending", created)
	}

	fetched, err := c.GetDatabase(context.Background(), "alpha")
	if err != nil {
		t.Fatal(err)
	}
	if fetched.Bucket != "alpha-bucket" || fetched.Hostname != "alpha.rad.example.com" || fetched.Namespace != "tenants" {
		t.Fatalf("fetched view = %+v", fetched)
	}

	if _, err := c.CreateDatabase(context.Background(), validSpec("alpha")); !errors.Is(err, ErrAlreadyExists) {
		t.Fatalf("duplicate create error = %v, want ErrAlreadyExists", err)
	}

	if err := c.DeleteDatabase(context.Background(), "alpha"); err != nil {
		t.Fatal(err)
	}
	if _, err := c.GetDatabase(context.Background(), "alpha"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("get after delete error = %v, want ErrNotFound", err)
	}
	if err := c.DeleteDatabase(context.Background(), "alpha"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("repeated delete error = %v, want ErrNotFound", err)
	}
}

func TestSpecValidationRejectsAmbiguousIdentity(t *testing.T) {
	c := testClient(t)
	for name, spec := range map[string]DatabaseSpec{
		"missing name": {Bucket: "b", CredentialsSecret: "s"},
		"no identity":  {Name: "x", Bucket: "b"},
		"both identities": {
			Name: "x", Bucket: "b",
			CredentialsSecret: "s", ServiceAccount: "sa",
		},
	} {
		if _, err := c.CreateDatabase(context.Background(), spec); err == nil {
			t.Errorf("%s: create accepted an invalid spec", name)
		}
	}
}

func TestServiceAccountIdentityIsProjected(t *testing.T) {
	c := testClient(t)
	spec := validSpec("sts")
	spec.CredentialsSecret = ""
	spec.ServiceAccount = "sts-identity"
	if _, err := c.CreateDatabase(context.Background(), spec); err != nil {
		t.Fatal(err)
	}
	resource := &radv1alpha1.Database{}
	if err := c.kube.Get(context.Background(),
		types.NamespacedName{Namespace: "tenants", Name: "sts"}, resource); err != nil {
		t.Fatal(err)
	}
	auth := resource.Spec.Storage.Authentication
	if auth.CredentialsSecretRef != nil || auth.ServiceAccountName == nil || *auth.ServiceAccountName != "sts-identity" {
		t.Fatalf("authentication = %+v", auth)
	}
}

func TestLoggingPolicyIsProjected(t *testing.T) {
	c := testClient(t)
	spec := validSpec("logging")
	spec.LogLevel = LogLevelDebug
	spec.LogFormat = LogFormatLogfmt
	spec.LogPrograms = true
	created, err := c.CreateDatabase(context.Background(), spec)
	if err != nil {
		t.Fatal(err)
	}
	if created.LogLevel != LogLevelDebug || created.LogFormat != LogFormatLogfmt || !created.LogPrograms {
		t.Fatalf("logging view = %+v", created)
	}
	resource := &radv1alpha1.Database{}
	if err := c.kube.Get(context.Background(), types.NamespacedName{Namespace: "tenants", Name: "logging"}, resource); err != nil {
		t.Fatal(err)
	}
	if resource.Spec.Logging.Level != radv1alpha1.LogLevelDebug || resource.Spec.Logging.Format != radv1alpha1.LogFormatLogfmt || !resource.Spec.Logging.Programs {
		t.Fatalf("logging resource = %+v", resource.Spec.Logging)
	}
}

func TestTelemetryPolicyIsProjected(t *testing.T) {
	c := testClient(t)
	spec := validSpec("telemetry")
	spec.OTelEndpoint = "http://collector:4318"
	spec.Diagnostics = DiagnosticLevelDetailed
	metrics := false
	spec.MetricsEnabled = &metrics
	spec.Slate.DecodedCacheSizeMiB = 256
	created, err := c.CreateDatabase(context.Background(), spec)
	if err != nil {
		t.Fatal(err)
	}
	if created.OTelEndpoint != spec.OTelEndpoint || created.Diagnostics != DiagnosticLevelDetailed || created.MetricsEnabled || created.Slate.DecodedCacheSizeMiB != 256 {
		t.Fatalf("telemetry view = %+v", created)
	}
	resource := &radv1alpha1.Database{}
	if err := c.kube.Get(context.Background(), types.NamespacedName{Namespace: "tenants", Name: "telemetry"}, resource); err != nil {
		t.Fatal(err)
	}
	if resource.Spec.Telemetry.Endpoint != spec.OTelEndpoint || resource.Spec.Telemetry.Diagnostics != radv1alpha1.DiagnosticLevelDetailed || resource.Spec.Telemetry.Metrics == nil || *resource.Spec.Telemetry.Metrics || resource.Spec.Slate.DecodedCacheSizeMiB != 256 {
		t.Fatalf("telemetry resource = %+v", resource.Spec.Telemetry)
	}
}

func TestMetricAndCacheDefaultsAreProjected(t *testing.T) {
	c := testClient(t)
	created, err := c.CreateDatabase(context.Background(), validSpec("metric-defaults"))
	if err != nil {
		t.Fatal(err)
	}
	if !created.MetricsEnabled || created.Slate.DecodedCacheSizeMiB != 128 {
		t.Fatalf("metric and cache defaults = %+v", created)
	}
}

func TestListIsNamespaceScoped(t *testing.T) {
	c := testClient(t)
	if _, err := c.CreateDatabase(context.Background(), validSpec("one")); err != nil {
		t.Fatal(err)
	}
	other := &radv1alpha1.Database{
		ObjectMeta: metav1.ObjectMeta{Namespace: "elsewhere", Name: "two"},
	}
	if err := c.kube.Create(context.Background(), other); err != nil {
		t.Fatal(err)
	}
	databases, err := c.ListDatabases(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(databases) != 1 || databases[0].Name != "one" {
		t.Fatalf("list = %+v, want only the client namespace's database", databases)
	}
}

func TestWaitReadyObservesTheReadyCondition(t *testing.T) {
	c := testClient(t)
	if _, err := c.CreateDatabase(context.Background(), validSpec("waiting")); err != nil {
		t.Fatal(err)
	}
	go func() {
		time.Sleep(50 * time.Millisecond)
		resource := &radv1alpha1.Database{}
		if err := c.kube.Get(context.Background(),
			types.NamespacedName{Namespace: "tenants", Name: "waiting"}, resource); err != nil {
			return
		}
		meta.SetStatusCondition(&resource.Status.Conditions, metav1.Condition{
			Type: radv1alpha1.ConditionReady, Status: metav1.ConditionTrue,
			Reason: "Available", Message: "database is ready",
		})
		resource.Status.URL = "https://waiting.rad.example.com"
		_ = c.kube.Status().Update(context.Background(), resource)
	}()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	database, err := c.WaitReady(ctx, "waiting")
	if err != nil {
		t.Fatal(err)
	}
	if !database.Ready || database.URL != "https://waiting.rad.example.com" {
		t.Fatalf("ready view = %+v", database)
	}
}

func TestWaitReadyTimeoutNamesTheUnsatisfiedDependency(t *testing.T) {
	c := testClient(t)
	if _, err := c.CreateDatabase(context.Background(), validSpec("stuck")); err != nil {
		t.Fatal(err)
	}
	resource := &radv1alpha1.Database{}
	if err := c.kube.Get(context.Background(),
		types.NamespacedName{Namespace: "tenants", Name: "stuck"}, resource); err != nil {
		t.Fatal(err)
	}
	meta.SetStatusCondition(&resource.Status.Conditions, metav1.Condition{
		Type: radv1alpha1.ConditionReady, Status: metav1.ConditionFalse,
		Reason: "SecretNotFound", Message: "credential Secret \"stuck-s3\" is missing",
	})
	if err := c.kube.Status().Update(context.Background(), resource); err != nil {
		t.Fatal(err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	_, err := c.WaitReady(ctx, "stuck")
	if err == nil || !strings.Contains(err.Error(), "SecretNotFound") {
		t.Fatalf("timeout error = %v, want the blocking dependency named", err)
	}
}
