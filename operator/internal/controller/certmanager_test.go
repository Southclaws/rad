package controller

import (
	"context"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

type fixedPresence bool

func (p fixedPresence) Available(context.Context) bool { return bool(p) }

// A Database with readers, cert-manager present or absent, reconciled to a
// fixpoint.
func tlsDatabase(
	t *testing.T,
	present bool,
	configure func(*radv1alpha1.Database),
) (*DatabaseReconciler, client.Client, *radv1alpha1.Database) {
	t.Helper()
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	if configure != nil {
		configure(database)
	}
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconciler.CertManager = fixedPresence(present)
	reconcileReadySpec(t, reconciler, database)
	return reconciler, kubernetesClient, database
}

func certManagerObject(t *testing.T, kubernetesClient client.Client, kind, name string) *unstructured.Unstructured {
	t.Helper()
	object := &unstructured.Unstructured{}
	object.SetGroupVersionKind(certManagerKind(kind))
	key := types.NamespacedName{Namespace: testNamespace, Name: name}
	if err := kubernetesClient.Get(context.Background(), key, object); err != nil {
		t.Fatalf("get %s/%s: %v", kind, name, err)
	}
	return object
}

func markCertificateReady(t *testing.T, kubernetesClient client.Client, name string) {
	t.Helper()
	certificate := certManagerObject(t, kubernetesClient, "Certificate", name)
	if err := unstructured.SetNestedSlice(certificate.Object, []any{
		map[string]any{"type": "Ready", "status": "True"},
	}, "status", "conditions"); err != nil {
		t.Fatal(err)
	}
	if err := kubernetesClient.Update(context.Background(), certificate); err != nil {
		t.Fatal(err)
	}
}

// The private-CA shape: a self-signed issuer mints an authority, and the
// authority issues the writer's certificate.
func TestCertManagerPresentProvisionsAPerDatabaseAuthority(t *testing.T) {
	reconciler, kubernetesClient, database := tlsDatabase(t, true, nil)

	selfSigned := certManagerObject(t, kubernetesClient, "Issuer", "rad-alpha-internal-selfsigned")
	if _, found, _ := unstructured.NestedMap(selfSigned.Object, "spec", "selfSigned"); !found {
		t.Fatalf("the bootstrap issuer is not self-signed: %#v", selfSigned.Object["spec"])
	}

	authority := certManagerObject(t, kubernetesClient, "Certificate", "rad-alpha-internal-ca")
	if isCA, _, _ := unstructured.NestedBool(authority.Object, "spec", "isCA"); !isCA {
		t.Fatal("the authority certificate is not a CA")
	}

	// The authority is namespace-scoped and per-database. A ClusterIssuer
	// would let one database's certificate authenticate as another's.
	authorityIssuer := certManagerObject(t, kubernetesClient, "Issuer", "rad-alpha-internal-ca")
	secret, _, _ := unstructured.NestedString(authorityIssuer.Object, "spec", "ca", "secretName")
	if secret != "rad-alpha-internal-ca" {
		t.Fatalf("the authority issuer signs from %q", secret)
	}

	writer := certManagerObject(t, kubernetesClient, "Certificate", "rad-alpha-internal")
	kind, _, _ := unstructured.NestedString(writer.Object, "spec", "issuerRef", "kind")
	name, _, _ := unstructured.NestedString(writer.Object, "spec", "issuerRef", "name")
	if kind != "Issuer" || name != "rad-alpha-internal-ca" {
		t.Fatalf("the writer certificate is issued by %s/%s", kind, name)
	}

	// Every name a reader could use must be on the certificate, so writer
	// election later changes the Service target and not the certificate.
	names, _, _ := unstructured.NestedStringSlice(writer.Object, "spec", "dnsNames")
	for _, want := range []string{
		"rad-alpha-internal",
		"rad-alpha-internal.default",
		"rad-alpha-internal.default.svc",
		"rad-alpha-internal.default.svc.cluster.local",
	} {
		found := false
		for _, name := range names {
			if name == want {
				found = true
			}
		}
		if !found {
			t.Fatalf("the writer certificate does not cover %q: %v", want, names)
		}
	}

	// An authority that rotated on the leaf's cadence would make every
	// verifying instance track it for no benefit, so it is given a life of
	// its own, and both renew before they expire rather than on
	// cert-manager's default.
	authorityDays := certificateDurationDays(t, authority)
	writerDays := certificateDurationDays(t, writer)
	if authorityDays <= writerDays*4 {
		t.Fatalf("authority lives %d days against a writer's %d", authorityDays, writerDays)
	}
	for name, object := range map[string]*unstructured.Unstructured{
		"authority": authority, "writer": writer,
	} {
		renew, found, _ := unstructured.NestedString(object.Object, "spec", "renewBefore")
		if !found || renew == "" {
			t.Fatalf("the %s certificate has no renewBefore", name)
		}
	}

	// Until the certificate is issued, the channel is not claimed to be ready.
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionInternalTLSReady)
	if condition == nil || condition.Status != metav1.ConditionFalse || condition.Reason != "Provisioning" {
		t.Fatalf("InternalTLSReady before issuance = %#v", condition)
	}

	markCertificateReady(t, kubernetesClient, "rad-alpha-internal")
	if _, err := reconciler.Reconcile(context.Background(),
		ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}

	observed = getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition = meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionInternalTLSReady)
	if condition == nil || condition.Status != metav1.ConditionTrue {
		t.Fatalf("InternalTLSReady after issuance = %#v", condition)
	}
	if observed.Status.InternalTransport == nil ||
		observed.Status.InternalTransport.Mode != "tls" ||
		observed.Status.InternalTransport.Provider != "cert-manager" {
		t.Fatalf("internal transport status = %#v", observed.Status.InternalTransport)
	}
	if got := observed.Status.InternalTransport.WriterAddress; got != "https://rad-alpha-internal.default.svc:7239" {
		t.Fatalf("writer address = %q", got)
	}

	assertTLSWiring(t, kubernetesClient, "rad-alpha-internal-tls")
}

// The writer serves the certificate and a reader verifies against the
// authority. A reader holding the writer's private key would hold the identity
// it exists to check.
func assertTLSWiring(t *testing.T, kubernetesClient client.Client, secret string) {
	t.Helper()
	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	writer := statefulSet.Spec.Template.Spec
	writerEnvironment := environmentMap(writer.Containers[0].Env)
	if writerEnvironment["RAD_INTERNAL_TLS_CERT"] != relayCertificatePath() {
		t.Fatalf("writer certificate path = %q", writerEnvironment["RAD_INTERNAL_TLS_CERT"])
	}
	if writerEnvironment["RAD_INTERNAL_TLS_KEY"] != relayCertificateKeyPath() {
		t.Fatalf("writer key path = %q", writerEnvironment["RAD_INTERNAL_TLS_KEY"])
	}

	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})
	reader := deployment.Spec.Template.Spec
	readerEnvironment := environmentMap(reader.Containers[0].Env)
	if readerEnvironment["RAD_RELAY_CA"] != relayAuthorityPath() {
		t.Fatalf("reader authority path = %q", readerEnvironment["RAD_RELAY_CA"])
	}
	if got := readerEnvironment["RAD_RELAY_TARGET"]; got != "https://rad-alpha-internal.default.svc:7239" {
		t.Fatalf("reader target = %q, want https", got)
	}
	if _, serving := readerEnvironment["RAD_INTERNAL_TLS_KEY"]; serving {
		t.Fatal("a reader was given a serving key")
	}

	for _, pair := range []struct {
		role string
		spec corev1.PodSpec
		keys []string
	}{
		{"writer", writer, []string{"tls.crt", "tls.key"}},
		{"reader", reader, []string{"ca.crt"}},
	} {
		var projected *corev1.SecretVolumeSource
		for index := range pair.spec.Volumes {
			volume := pair.spec.Volumes[index]
			if volume.Secret != nil && volume.Secret.SecretName == secret {
				projected = volume.Secret
			}
		}
		if projected == nil {
			t.Fatalf("%s does not mount %q: %#v", pair.role, secret, pair.spec.Volumes)
		}
		if len(projected.Items) != len(pair.keys) {
			t.Fatalf("%s projects %#v, want exactly %v", pair.role, projected.Items, pair.keys)
		}
		for index, key := range pair.keys {
			if projected.Items[index].Key != key {
				t.Fatalf("%s projects %q, want %q", pair.role, projected.Items[index].Key, key)
			}
		}
	}
}

// Without cert-manager the channel stays authenticated but unencrypted rather
// than failing. That is the development posture, and it is stated in the
// condition rather than left to be inferred.
func TestCertManagerAbsentUnderAutoFallsBackToPlaintext(t *testing.T) {
	_, kubernetesClient, database := tlsDatabase(t, false, nil)

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionInternalTLSReady)
	if condition == nil || condition.Status != metav1.ConditionFalse || condition.Reason != "Plaintext" {
		t.Fatalf("InternalTLSReady = %#v", condition)
	}
	// The database is still available: the channel is advisory.
	if ready := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionReady); ready == nil {
		t.Fatal("no Ready condition at all")
	}
	if observed.Status.InternalTransport.Mode != "plaintext" {
		t.Fatalf("transport = %#v", observed.Status.InternalTransport)
	}

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	environment := environmentMap(statefulSet.Spec.Template.Spec.Containers[0].Env)
	if _, serving := environment["RAD_INTERNAL_TLS_CERT"]; serving {
		t.Fatal("the writer was told to serve a certificate that does not exist")
	}
}

// `required` is the promise that the channel is never unencrypted. It must
// hold the database rather than quietly downgrade.
func TestCertManagerAbsentUnderRequiredHoldsWithAUsefulReason(t *testing.T) {
	_, kubernetesClient, database := tlsDatabase(t, false, func(database *radv1alpha1.Database) {
		database.Spec.InternalTLS.Mode = radv1alpha1.InternalTLSRequired
	})

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionInternalTLSReady)
	if condition == nil || condition.Status != metav1.ConditionFalse {
		t.Fatalf("InternalTLSReady = %#v", condition)
	}
	if condition.Reason != "InternalTLSUnavailable" {
		t.Fatalf("reason = %q", condition.Reason)
	}
	// A reason that does not say what to install is not worth publishing.
	if condition.Message == "" {
		t.Fatal("the condition does not say why")
	}
}

// A supplied certificate is used as it is, and the operator creates no
// authority of its own for a database that already has one.
func TestASuppliedCertificateWinsAndIsNotOwned(t *testing.T) {
	supplied := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: "my-internal-tls", Namespace: testNamespace},
		Type:       corev1.SecretTypeTLS,
		Data: map[string][]byte{
			"tls.crt": []byte("certificate"),
			"tls.key": []byte("key"),
			"ca.crt":  []byte("authority"),
		},
	}
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	database.Spec.InternalTLS.SecretName = "my-internal-tls"
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"), supplied)
	reconciler.CertManager = fixedPresence(true)
	reconcileReadySpec(t, reconciler, database)

	// cert-manager is installed, and still nothing was asked of it.
	for _, kind := range []string{"Issuer", "Certificate"} {
		object := &unstructured.Unstructured{}
		object.SetGroupVersionKind(certManagerKind(kind))
		key := types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}
		if err := kubernetesClient.Get(context.Background(), key, object); !apierrors.IsNotFound(err) {
			t.Fatalf("a %s was created despite a supplied certificate: %v", kind, err)
		}
	}

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	if observed.Status.InternalTransport.Provider != "user" {
		t.Fatalf("provider = %#v", observed.Status.InternalTransport)
	}
	assertTLSWiring(t, kubernetesClient, "my-internal-tls")

	// The operator validated the Secret; it must not have adopted it.
	current := getObject(t, kubernetesClient, client.ObjectKeyFromObject(supplied), &corev1.Secret{})
	if len(current.OwnerReferences) != 0 {
		t.Fatalf("the supplied Secret was adopted: %#v", current.OwnerReferences)
	}
}

func TestAnIncompleteSuppliedCertificateIsRefused(t *testing.T) {
	supplied := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: "my-internal-tls", Namespace: testNamespace},
		Type:       corev1.SecretTypeTLS,
		Data:       map[string][]byte{"tls.crt": []byte("certificate")},
	}
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	database.Spec.InternalTLS.Mode = radv1alpha1.InternalTLSRequired
	database.Spec.InternalTLS.SecretName = "my-internal-tls"
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"), supplied)
	reconciler.CertManager = fixedPresence(false)
	reconcileReadySpec(t, reconciler, database)

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionInternalTLSReady)
	if condition == nil || condition.Status != metav1.ConditionFalse {
		t.Fatalf("InternalTLSReady = %#v", condition)
	}
}

func TestAnIncompleteSuppliedCertificateDoesNotSelectPlaintextInAutoMode(t *testing.T) {
	supplied := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: "my-internal-tls", Namespace: testNamespace},
		Type:       corev1.SecretTypeTLS,
		Data:       map[string][]byte{"tls.crt": []byte("certificate")},
	}
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	database.Spec.InternalTLS.SecretName = "my-internal-tls"
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"), supplied)
	reconciler.CertManager = fixedPresence(false)
	reconcileReadySpec(t, reconciler, database)

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	if observed.Status.InternalTransport == nil || observed.Status.InternalTransport.Mode != "tls" {
		t.Fatalf("invalid supplied TLS selected plaintext: %#v", observed.Status.InternalTransport)
	}
	if observed.Status.InternalTransport.Provider != "user" {
		t.Fatalf("provider = %#v", observed.Status.InternalTransport)
	}
	assertTLSWiring(t, kubernetesClient, "my-internal-tls")
}

func TestRequiredTLSRemovesAnExistingPlaintextWorkload(t *testing.T) {
	reconciler, kubernetesClient, database := tlsDatabase(t, false, nil)
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})

	current := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	current.Spec.InternalTLS.Mode = radv1alpha1.InternalTLSRequired
	current.Generation++
	if err := kubernetesClient.Update(context.Background(), current); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{
		NamespacedName: client.ObjectKeyFromObject(database),
	}); err != nil {
		t.Fatal(err)
	}

	for _, workload := range []struct {
		name   string
		object client.Object
	}{
		{"rad-alpha", &appsv1.StatefulSet{}},
		{"rad-alpha-reader", &appsv1.Deployment{}},
	} {
		err := kubernetesClient.Get(context.Background(), types.NamespacedName{
			Namespace: testNamespace,
			Name:      workload.name,
		}, workload.object)
		if !apierrors.IsNotFound(err) {
			t.Fatalf("%s still exists after required TLS became unavailable: %v", workload.name, err)
		}
	}
}

// A deployment with its own PKI points at its issuer, and the operator builds
// no authority of its own.
func TestAnExternalIssuerReplacesThePerDatabaseAuthority(t *testing.T) {
	_, kubernetesClient, _ := tlsDatabase(t, true, func(database *radv1alpha1.Database) {
		database.Spec.InternalTLS.IssuerRef = &radv1alpha1.IssuerReference{
			Name: "corporate-pki", Kind: "ClusterIssuer", Group: certManagerGroup,
		}
	})

	writer := certManagerObject(t, kubernetesClient, "Certificate", "rad-alpha-internal")
	kind, _, _ := unstructured.NestedString(writer.Object, "spec", "issuerRef", "kind")
	name, _, _ := unstructured.NestedString(writer.Object, "spec", "issuerRef", "name")
	if kind != "ClusterIssuer" || name != "corporate-pki" {
		t.Fatalf("the writer certificate is issued by %s/%s", kind, name)
	}
	for _, unwanted := range []string{"rad-alpha-internal-selfsigned", "rad-alpha-internal-ca"} {
		object := &unstructured.Unstructured{}
		object.SetGroupVersionKind(certManagerKind("Issuer"))
		key := types.NamespacedName{Namespace: testNamespace, Name: unwanted}
		if err := kubernetesClient.Get(context.Background(), key, object); !apierrors.IsNotFound(err) {
			t.Fatalf("%s was created despite an external issuer: %v", unwanted, err)
		}
	}
}

// No readers means no channel, so there is nothing to secure and nothing to
// report as insecure.
func TestNoReadersLeavesNothingToSecure(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconciler.CertManager = fixedPresence(true)
	reconcileReadySpec(t, reconciler, database)

	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionInternalTLSReady)
	if condition == nil || condition.Status != metav1.ConditionTrue || condition.Reason != "NotApplicable" {
		t.Fatalf("InternalTLSReady = %#v", condition)
	}
	if observed.Status.InternalTransport != nil {
		t.Fatalf("transport reported without a channel: %#v", observed.Status.InternalTransport)
	}
}

// Discovery is asked once. The answer changes only when somebody installs or
// removes cert-manager, so a request per database per loop would be waste.
func TestCertManagerDiscoveryIsCachedButNotCachedOnFailure(t *testing.T) {
	calls := 0
	present := &DiscoveredCertManager{Discovery: func() ([]schema.GroupVersion, error) {
		calls++
		return []schema.GroupVersion{
			{Group: "apps", Version: "v1"},
			{Group: certManagerGroup, Version: certManagerVersion},
		}, nil
	}}
	for range 5 {
		if !present.Available(context.Background()) {
			t.Fatal("cert-manager was not detected")
		}
	}
	if calls != 1 {
		t.Fatalf("discovery ran %d times, want once", calls)
	}

	absent := &DiscoveredCertManager{Discovery: func() ([]schema.GroupVersion, error) {
		calls++
		return []schema.GroupVersion{{Group: "apps", Version: "v1"}}, nil
	}}
	calls = 0
	for range 3 {
		if absent.Available(context.Background()) {
			t.Fatal("cert-manager was detected without its API")
		}
	}
	if calls != 1 {
		t.Fatalf("discovery ran %d times for an absent API, want once", calls)
	}

	// An unanswerable call is not evidence of absence, so it is asked again.
	calls = 0
	failing := &DiscoveredCertManager{Discovery: func() ([]schema.GroupVersion, error) {
		calls++
		return nil, context.DeadlineExceeded
	}}
	for range 3 {
		failing.Available(context.Background())
	}
	if calls != 3 {
		t.Fatalf("a failed discovery was cached after %d calls", calls)
	}
}

// Whole days in a cert-manager duration, which is a Go duration string.
func certificateDurationDays(t *testing.T, object *unstructured.Unstructured) int {
	t.Helper()
	raw, found, _ := unstructured.NestedString(object.Object, "spec", "duration")
	if !found || raw == "" {
		t.Fatalf("%s has no explicit duration, so it takes cert-manager's default", object.GetName())
	}
	parsed, err := time.ParseDuration(raw)
	if err != nil {
		t.Fatalf("duration %q: %v", raw, err)
	}
	return int(parsed.Hours() / 24)
}
