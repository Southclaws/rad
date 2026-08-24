package controller

import (
	"context"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func relayDatabase(t *testing.T, readers int32) (*DatabaseReconciler, client.Client, *radv1alpha1.Database) {
	t.Helper()
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = readers
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)
	return reconciler, kubernetesClient, database
}

func TestRelayGivesTheWriterAListenerAndReadersATarget(t *testing.T) {
	_, kubernetesClient, _ := relayDatabase(t, 2)

	secret := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}, &corev1.Secret{})
	if len(secret.Data[relayTokenKey]) < 32 {
		t.Fatalf("relay token is %d bytes, too few to be a secret", len(secret.Data[relayTokenKey]))
	}

	service := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}, &corev1.Service{})
	if service.Spec.Selector[roleLabel] != writeRole {
		t.Fatalf("the internal service selects %q, want the writer", service.Spec.Selector[roleLabel])
	}
	if service.Spec.Ports[0].Port != internalPort {
		t.Fatalf("internal service ports = %#v", service.Spec.Ports)
	}

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	writer := statefulSet.Spec.Template.Spec.Containers[0]
	writerEnvironment := environmentMap(writer.Env)
	if got := writerEnvironment["RAD_INTERNAL_ADDR"]; got != "0.0.0.0:7239" {
		t.Fatalf("writer RAD_INTERNAL_ADDR = %q", got)
	}
	if _, relaying := writerEnvironment["RAD_RELAY_TARGET"]; relaying {
		t.Fatal("the writer was told to relay to something")
	}

	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})
	reader := deployment.Spec.Template.Spec.Containers[0]
	readerEnvironment := environmentMap(reader.Env)
	if got := readerEnvironment["RAD_RELAY_TARGET"]; got != "http://rad-alpha-internal.default.svc:7239" {
		t.Fatalf("reader RAD_RELAY_TARGET = %q", got)
	}
	if _, listening := readerEnvironment["RAD_INTERNAL_ADDR"]; listening {
		t.Fatal("a reader was told to serve the internal API")
	}

	// The secret is a file in both, never an argument or an inline value:
	// argv and the environment are readable in ways a mounted file is not.
	for _, pair := range []struct {
		role      string
		container corev1.Container
		spec      corev1.PodSpec
	}{
		{"writer", writer, statefulSet.Spec.Template.Spec},
		{"reader", reader, deployment.Spec.Template.Spec},
	} {
		environment := environmentMap(pair.container.Env)
		if environment["RAD_RELAY_TOKEN_FILE"] != relayTokenPath() {
			t.Fatalf("%s token file = %q", pair.role, environment["RAD_RELAY_TOKEN_FILE"])
		}
		for name, value := range environment {
			if value == string(secret.Data[relayTokenKey]) {
				t.Fatalf("%s carries the token inline in %s", pair.role, name)
			}
		}
		if len(pair.container.VolumeMounts) != 1 || pair.container.VolumeMounts[0].MountPath != relayMountPath {
			t.Fatalf("%s mounts = %#v", pair.role, pair.container.VolumeMounts)
		}
		if !pair.container.VolumeMounts[0].ReadOnly {
			t.Fatalf("%s mounts the token writable", pair.role)
		}
		if len(pair.spec.Volumes) != 1 || pair.spec.Volumes[0].Secret == nil {
			t.Fatalf("%s volumes = %#v", pair.role, pair.spec.Volumes)
		}
		if pair.spec.Volumes[0].Secret.SecretName != "rad-alpha-internal" {
			t.Fatalf("%s mounts %q", pair.role, pair.spec.Volumes[0].Secret.SecretName)
		}
		assertTokenReadableByItsProcess(t, pair.role, pair.container, pair.spec)
	}

	// Only the writer serves the port, so only the writer declares it.
	if len(writer.Ports) != 2 || writer.Ports[1].ContainerPort != internalPort {
		t.Fatalf("writer ports = %#v", writer.Ports)
	}
	if len(reader.Ports) != 1 {
		t.Fatalf("reader ports = %#v, want only the client port", reader.Ports)
	}
}

func TestCorpusCapturePolicyReachesWriterAndReaders(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Readers = 2
	database.Spec.CaptureWorkloadCorpus = true
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})
	for role, environment := range map[string][]corev1.EnvVar{
		"writer": statefulSet.Spec.Template.Spec.Containers[0].Env,
		"reader": deployment.Spec.Template.Spec.Containers[0].Env,
	} {
		if environmentMap(environment)["RAD_CAPTURE_WORKLOAD_CORPUS"] != "true" {
			t.Fatalf("%s does not receive the corpus capture policy", role)
		}
	}
}

// A mounted Secret is owned by root, and the container runs unprivileged, so a
// file mode alone does not make the token readable. Getting this wrong is
// invisible until the process starts and fails to read its own secret.
func assertTokenReadableByItsProcess(
	t *testing.T,
	role string,
	container corev1.Container,
	spec corev1.PodSpec,
) {
	t.Helper()
	user := container.SecurityContext.RunAsUser
	if user == nil {
		t.Fatalf("%s does not pin a runtime user", role)
	}
	mode := spec.Volumes[0].Secret.DefaultMode
	if mode == nil {
		t.Fatalf("%s mounts the token with no explicit mode", role)
	}
	const ownerRead, groupRead, otherRead = 0o400, 0o040, 0o004
	switch {
	case *mode&otherRead != 0:
		// Readable regardless of ownership, which is permissive but works.
	case *mode&groupRead != 0:
		if spec.SecurityContext == nil || spec.SecurityContext.FSGroup == nil {
			t.Fatalf("%s relies on group read with no fsGroup to own the mount", role)
		}
		if *spec.SecurityContext.FSGroup != *user {
			t.Fatalf(
				"%s runs as %d but the mount is owned by group %d",
				role, *user, *spec.SecurityContext.FSGroup,
			)
		}
	case *mode&ownerRead != 0:
		// Owner is root on a Secret volume, and the process is not root.
		t.Fatalf(
			"%s mounts the token %#o, readable only by its root owner, but runs as %d",
			role, *mode, *user,
		)
	default:
		t.Fatalf("%s mounts the token %#o, readable by nobody", role, *mode)
	}
}

// Rotating the token on every reconcile would break every reader on every
// loop. It is generated once and adopted thereafter.
func TestTheRelaySecretIsGeneratedOnceAndThenAdopted(t *testing.T) {
	reconciler, kubernetesClient, database := relayDatabase(t, 2)
	key := types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}
	first := getObject(t, kubernetesClient, key, &corev1.Secret{})

	for range 3 {
		if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
			t.Fatal(err)
		}
	}
	again := getObject(t, kubernetesClient, key, &corev1.Secret{})
	if string(again.Data[relayTokenKey]) != string(first.Data[relayTokenKey]) {
		t.Fatal("reconciling rotated the relay token")
	}
	if again.ResourceVersion != first.ResourceVersion {
		t.Fatalf("the relay Secret was rewritten: %s -> %s", first.ResourceVersion, again.ResourceVersion)
	}
}

// The relay carries reader evidence, so without readers there is nothing for
// it to carry and no reason to hold a secret.
func TestNoReadersMeansNoRelay(t *testing.T) {
	_, kubernetesClient, _ := relayDatabase(t, 0)
	key := types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}
	for _, object := range []client.Object{&corev1.Secret{}, &corev1.Service{}} {
		if err := kubernetesClient.Get(context.Background(), key, object); !apierrors.IsNotFound(err) {
			t.Fatalf("%T exists without readers: %v", object, err)
		}
	}
	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	container := statefulSet.Spec.Template.Spec.Containers[0]
	if _, listening := environmentMap(container.Env)["RAD_INTERNAL_ADDR"]; listening {
		t.Fatal("the writer serves the internal API with no readers to serve")
	}
	if len(container.VolumeMounts) != 0 {
		t.Fatalf("writer mounts = %#v, want none", container.VolumeMounts)
	}
}

// The gateway may reach the client port. Only this database's own readers may
// reach the internal one.
func TestTheNetworkPolicyOpensTheInternalPortOnlyToReaders(t *testing.T) {
	_, kubernetesClient, _ := relayDatabase(t, 2)
	policy := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &networkingv1.NetworkPolicy{})
	if len(policy.Spec.Ingress) != 2 {
		t.Fatalf("network policy rules = %#v", policy.Spec.Ingress)
	}
	internal := policy.Spec.Ingress[1]
	if internal.Ports[0].Port.IntValue() != internalPort {
		t.Fatalf("second rule opens %#v", internal.Ports)
	}
	if len(internal.From) != 1 || internal.From[0].PodSelector == nil {
		t.Fatalf("internal rule peers = %#v", internal.From)
	}
	if got := internal.From[0].PodSelector.MatchLabels[roleLabel]; got != readRole {
		t.Fatalf("internal rule admits role %q, want readers only", got)
	}
	if internal.From[0].NamespaceSelector != nil {
		t.Fatal("the internal rule admits another namespace")
	}
	// The gateway rule must not have gained the internal port with it.
	for _, port := range policy.Spec.Ingress[0].Ports {
		if port.Port.IntValue() == internalPort {
			t.Fatal("the gateway can reach the internal port")
		}
	}
}

func TestScalingReadersToZeroRemovesTheRelay(t *testing.T) {
	reconciler, kubernetesClient, database := relayDatabase(t, 2)
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}, &corev1.Secret{})

	scaled := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	scaled.Spec.Readers = 0
	if err := kubernetesClient.Update(context.Background(), scaled); err != nil {
		t.Fatal(err)
	}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: client.ObjectKeyFromObject(database)}); err != nil {
		t.Fatal(err)
	}
	key := types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}
	for _, object := range []client.Object{&corev1.Secret{}, &corev1.Service{}} {
		if err := kubernetesClient.Get(context.Background(), key, object); !apierrors.IsNotFound(err) {
			t.Fatalf("%T survived scaling readers to zero: %v", object, err)
		}
	}
}

func TestRelayResourceNamesRemainDNSLabels(t *testing.T) {
	name := "tenant-with-a-name-that-is-deliberately-longer-than-a-derived-resource-allows"
	got := relayResourceName(name)
	if len(got) > 63 || got != relayResourceName(name) {
		t.Fatalf("relay resource name = %q", got)
	}
	for _, other := range []string{readerResourceName(name), headlessServiceName(name)} {
		if got == other {
			t.Fatalf("truncated relay name collides with %q", other)
		}
	}
	if got := relayResourceName("alpha"); got != "rad-alpha-internal" {
		t.Fatalf("relay resource name = %q", got)
	}
}

func TestDeleteRemovesTheRelaySecret(t *testing.T) {
	reconciler, kubernetesClient, database := relayDatabase(t, 2)
	current := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	current.DeletionTimestamp = &metav1.Time{Time: time.Unix(2, 0)}
	current.Finalizers = []string{finalizerName}

	for attempt := 0; attempt < 12; attempt++ {
		if _, err := reconciler.reconcileDelete(context.Background(), current); err != nil {
			t.Fatal(err)
		}
		err := kubernetesClient.Get(context.Background(),
			types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-internal"}, &corev1.Secret{})
		if apierrors.IsNotFound(err) {
			return
		}
	}
	t.Fatal("the relay Secret survived deletion")
}
