//go:build e2e

package e2e

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func certManagerGVK(kind string) schema.GroupVersionKind {
	return schema.GroupVersionKind{Group: "cert-manager.io", Version: "v1", Kind: kind}
}

// The secure path is only meaningful where cert-manager can issue. A cluster
// without it skips rather than fails, so a developer without cert-manager can
// still run the suite; CI installs it, so there the path always runs.
func requireCertManager(t *testing.T, h *harness) {
	t.Helper()
	list := &unstructured.UnstructuredList{}
	list.SetGroupVersionKind(certManagerGVK("IssuerList"))
	if err := h.client.List(context.Background(), list, client.InNamespace(h.namespace)); err != nil {
		if meta.IsNoMatchError(err) || apierrors.IsNotFound(err) {
			t.Skip("cert-manager is not installed; run task certmanager:install")
		}
		t.Fatal(err)
	}
}

func (h *harness) certManagerResource(t *testing.T, kind, name string) *unstructured.Unstructured {
	t.Helper()
	object := &unstructured.Unstructured{}
	object.SetGroupVersionKind(certManagerGVK(kind))
	key := types.NamespacedName{Namespace: h.namespace, Name: name}
	if err := h.client.Get(context.Background(), key, object); err != nil {
		t.Fatalf("get %s/%s: %v", kind, name, err)
	}
	return object
}

// The certificate the writer is actually serving, as PEM.
//
// Read from the connection rather than from the Secret: what a Secret holds
// and what a running process serves are exactly the two things a renewal can
// put out of step.
func (h *harness) servedCertificate(t *testing.T, database string) string {
	t.Helper()
	url := fmt.Sprintf("https://rad-%s-internal.%s.svc:7239/internal/livez", database, h.namespace)
	output, err := h.curl(t, "-k", "-o", "/dev/null", "--write-out", "%{certs}", url)
	if err != nil {
		t.Fatalf("read the served certificate: %v (%s)", err, output)
	}
	if !strings.Contains(output, "BEGIN CERTIFICATE") {
		t.Fatalf("no certificate was served: %s", output)
	}
	return output
}

func (h *harness) storedCertificate(t *testing.T, secretName string) string {
	t.Helper()
	secret := &corev1.Secret{}
	key := types.NamespacedName{Namespace: h.namespace, Name: secretName}
	if err := h.client.Get(context.Background(), key, secret); err != nil {
		t.Fatalf("get TLS Secret: %v", err)
	}
	return string(secret.Data["tls.crt"])
}

func (h *harness) newTLSDatabase(name string) *radv1alpha1.Database {
	database := h.newDatabase(name, name, name)
	database.Spec.Readers = 2
	// `required` rather than `auto`: a fallback to plaintext would make this
	// suite pass while proving nothing about TLS.
	database.Spec.InternalTLS = radv1alpha1.InternalTLS{Mode: radv1alpha1.InternalTLSRequired}
	return database
}

// The whole secure path in anger: cert-manager issues from a per-database
// authority, the writer serves it, and readers relay their evidence over it.
func TestInternalTLSIsProvisionedAndCarriesReaderEvidence(t *testing.T) {
	h := newHarness(t)
	requireCertManager(t, h)
	name := h.name("e2e-tls")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newTLSDatabase(name))
	h.waitReady(t, name)
	h.waitCondition(t, name, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionTrue, "Available", 5*time.Minute)
	h.waitReadyReaders(t, name, 2)

	// The private-CA chain, and a writer certificate issued from it.
	for _, resource := range []struct{ kind, suffix string }{
		{"Issuer", "-internal-selfsigned"},
		{"Certificate", "-internal-ca"},
		{"Issuer", "-internal-ca"},
		{"Certificate", "-internal"},
	} {
		object := h.certManagerResource(t, resource.kind, "rad"+"-"+name+resource.suffix)
		if !certManagerReady(object) {
			t.Fatalf("%s %s is not ready: %v", resource.kind, object.GetName(), object.Object["status"])
		}
	}

	// The writer serves what cert-manager issued, not something else.
	secretName := "rad-" + name + "-internal-tls"
	served := h.servedCertificate(t, name)
	if stored := h.storedCertificate(t, secretName); !strings.Contains(served, strings.TrimSpace(stored)) &&
		!strings.Contains(strings.TrimSpace(stored), strings.TrimSpace(served)) {
		t.Fatalf("the writer serves a certificate that is not the issued one")
	}

	// Status describes the channel without anyone having to inspect pods.
	database := &radv1alpha1.Database{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: name}, database); err != nil {
		t.Fatal(err)
	}
	transport := database.Status.InternalTransport
	if transport == nil || transport.Mode != "tls" || transport.Provider != "cert-manager" {
		t.Fatalf("internal transport = %#v", transport)
	}
	if !strings.HasPrefix(transport.WriterAddress, "https://") {
		t.Fatalf("writer address = %q, want https", transport.WriterAddress)
	}

	// Evidence must cross the encrypted channel. Only the readers serve these
	// queries, so the writer can hold them by no other route.
	h.seedMarker(t, name, "tls-marker")
	baseline := h.writerExecutions(t, name)
	for range 20 {
		if _, err := h.curlPost(t, h.readerServiceURL(name), "/execute", queryBody("markers")); err != nil {
			t.Fatal(err)
		}
	}
	h.waitForRelayedExecutions(t, name, baseline+20)

	// TLS proves who the writer is; it does not decide who may submit.
	code, err := h.curl(t, "-k", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST",
		"-H", "content-type: application/json", "-d", `{"format":1}`,
		fmt.Sprintf("https://rad-%s-internal.%s.svc:7239/internal/statistics/observations", name, h.namespace))
	if err != nil || strings.TrimSpace(code) != "401" {
		t.Fatalf("unauthenticated submission over TLS = %q (%v), want 401", code, err)
	}
}

// The renewal trap: cert-manager rewrites the Secret, and a process that read
// its certificate once would serve the old one until something restarted it.
func TestARenewedCertificateIsServedWithoutRestartingTheWriter(t *testing.T) {
	h := newHarness(t)
	requireCertManager(t, h)
	name := h.name("e2e-tls-renew")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newTLSDatabase(name))
	h.waitReady(t, name)
	h.waitCondition(t, name, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionTrue, "Available", 5*time.Minute)
	h.waitReadyReaders(t, name, 2)

	secretName := "rad-" + name + "-internal-tls"
	before := h.servedCertificate(t, name)
	restartsBefore := h.podRestarts(t, h.writerPod(name))

	// Deleting the Secret makes cert-manager issue again, which is what a
	// renewal does to the mounted files without waiting ninety days for one.
	if err := h.client.Delete(context.Background(), &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: secretName, Namespace: h.namespace},
	}); err != nil {
		t.Fatal(err)
	}
	waitFor(t, 3*time.Minute, "cert-manager to reissue the certificate", func() (bool, string) {
		secret := &corev1.Secret{}
		err := h.client.Get(context.Background(),
			types.NamespacedName{Namespace: h.namespace, Name: secretName}, secret)
		if err != nil {
			return false, err.Error()
		}
		stored := strings.TrimSpace(string(secret.Data["tls.crt"]))
		return stored != "" && !strings.Contains(before, stored), "unchanged"
	})

	// The kubelet republishes the mounted Secret on its own sync period, and
	// Rad re-reads the files on its own interval, so the wait covers both.
	waitFor(t, 6*time.Minute, "the writer to serve the renewed certificate", func() (bool, string) {
		served := h.servedCertificate(t, name)
		if served == before {
			return false, "still serving the previous certificate"
		}
		stored := strings.TrimSpace(h.storedCertificate(t, secretName))
		if !strings.Contains(served, stored) && !strings.Contains(stored, strings.TrimSpace(served)) {
			return false, "serving neither the old nor the stored certificate"
		}
		return true, ""
	})

	// The point of reloading is that nothing had to be restarted for it.
	if restartsAfter := h.podRestarts(t, h.writerPod(name)); restartsAfter != restartsBefore {
		t.Fatalf("the writer restarted for a renewal: %d -> %d", restartsBefore, restartsAfter)
	}

	// And the channel still carries evidence afterwards.
	h.seedMarker(t, name, "renewed-marker")
	baseline := h.writerExecutions(t, name)
	for range 20 {
		if _, err := h.curlPost(t, h.readerServiceURL(name), "/execute", queryBody("markers")); err != nil {
			t.Fatal(err)
		}
	}
	h.waitForRelayedExecutions(t, name, baseline+20)
}

// Deleting the database removes the PKI it owns. A certificate authority left
// behind would outlive the only thing that could ever use it.
func TestDeletingTheDatabaseRemovesTheCertificatesItOwns(t *testing.T) {
	h := newHarness(t)
	requireCertManager(t, h)
	name := h.name("e2e-tls-gc")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	database := h.newTLSDatabase(name)
	h.createDatabase(t, database)
	h.waitReady(t, name)
	h.waitCondition(t, name, radv1alpha1.ConditionInternalTLSReady, metav1.ConditionTrue, "Available", 5*time.Minute)

	h.certManagerResource(t, "Certificate", "rad-"+name+"-internal")
	h.deleteIgnoreMissing(database)
	h.waitGone(t, database)

	for _, resource := range []struct{ kind, suffix string }{
		{"Issuer", "-internal-selfsigned"},
		{"Certificate", "-internal-ca"},
		{"Issuer", "-internal-ca"},
		{"Certificate", "-internal"},
	} {
		object := &unstructured.Unstructured{}
		object.SetGroupVersionKind(certManagerGVK(resource.kind))
		key := types.NamespacedName{Namespace: h.namespace, Name: "rad-" + name + resource.suffix}
		waitFor(t, 2*time.Minute, resource.kind+resource.suffix+" removal", func() (bool, string) {
			err := h.client.Get(context.Background(), key, object)
			if apierrors.IsNotFound(err) {
				return true, ""
			}
			if err != nil {
				return false, err.Error()
			}
			return false, "still present"
		})
	}
}

func certManagerReady(object *unstructured.Unstructured) bool {
	conditions, found, err := unstructured.NestedSlice(object.Object, "status", "conditions")
	if err != nil || !found {
		return false
	}
	for _, entry := range conditions {
		condition, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		if condition["type"] == "Ready" && condition["status"] == "True" {
			return true
		}
	}
	return false
}

func (h *harness) writerExecutions(t *testing.T, database string) int {
	t.Helper()
	body, err := h.curl(t, h.serviceURL(database)+"/statistics")
	if err != nil {
		t.Fatalf("read statistics: %v", err)
	}
	return totalRetainedExecutions(body)
}

func (h *harness) waitForRelayedExecutions(t *testing.T, database string, want int) {
	t.Helper()
	waitFor(t, 3*time.Minute, fmt.Sprintf("%d relayed executions", want), func() (bool, string) {
		body, err := h.curl(t, h.serviceURL(database)+"/statistics")
		if err != nil {
			return false, err.Error()
		}
		got := totalRetainedExecutions(body)
		return got >= want, fmt.Sprintf("%d/%d", got, want)
	})
}
