package controller

import (
	"context"
	"fmt"
	"sync/atomic"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

const (
	certManagerGroup   = "cert-manager.io"
	certManagerVersion = "v1"

	// An authority outlives the certificates it signs by a wide margin.
	// cert-manager's ninety-day default would rotate the trust anchor on the
	// same cadence as a leaf, which every instance verifying against it would
	// have to keep up with for no benefit.
	authorityDuration    = "43800h" // five years
	authorityRenewBefore = "8760h"  // one year
	// The writer's own certificate renews often, because renewing it is free:
	// it reloads without a restart.
	writerDuration    = "2160h" // ninety days
	writerRenewBefore = "720h"  // thirty days
)

func certManagerKind(kind string) schema.GroupVersionKind {
	return schema.GroupVersionKind{Group: certManagerGroup, Version: certManagerVersion, Kind: kind}
}

// CertManagerPresence answers whether cert-manager's API is served here.
//
// The question is about API capability, not about a Deployment: cert-manager
// may be installed in any namespace under any name, and its resources are
// usable exactly when the API server serves them.
type CertManagerPresence interface {
	Available(ctx context.Context) bool
}

// DiscoveredCertManager caches the discovery result. The answer changes only
// when someone installs or removes cert-manager, so asking the API server on
// every reconcile would be a request per database per loop for a fact that
// almost never moves.
type DiscoveredCertManager struct {
	Discovery func() ([]schema.GroupVersion, error)
	resolved  atomic.Bool
	present   atomic.Bool
}

func (d *DiscoveredCertManager) Available(context.Context) bool {
	if d.resolved.Load() {
		return d.present.Load()
	}
	groups, err := d.Discovery()
	if err != nil {
		// An unanswerable discovery call is not evidence of absence, so it is
		// not cached: the next reconcile asks again.
		return false
	}
	present := false
	for _, group := range groups {
		if group.Group == certManagerGroup && group.Version == certManagerVersion {
			present = true
			break
		}
	}
	d.present.Store(present)
	d.resolved.Store(true)
	return present
}

// certificateSource is where the writer's serving certificate comes from.
type certificateSource int

const (
	// certificateAbsent serves the internal channel over plain HTTP.
	certificateAbsent certificateSource = iota
	// certificateUserSupplied uses a Secret the operator does not own.
	certificateUserSupplied
	// certificateManaged uses one cert-manager issues on the operator's behalf.
	certificateManaged
)

type internalTransport struct {
	source certificateSource
	// secret holds tls.crt, tls.key, and ca.crt once it exists.
	secret string
	// ready is false while a managed certificate is still being issued.
	ready   bool
	pending string
}

func (t internalTransport) tls() bool {
	return t.source != certificateAbsent
}

func (t internalTransport) provider() string {
	switch t.source {
	case certificateUserSupplied:
		return "user"
	case certificateManaged:
		return "cert-manager"
	default:
		return "none"
	}
}

func internalTLSMode(database *radv1alpha1.Database) radv1alpha1.InternalTLSMode {
	if database.Spec.InternalTLS.Mode == "" {
		return radv1alpha1.InternalTLSAuto
	}
	return database.Spec.InternalTLS.Mode
}

// resolveInternalTLS decides where the certificate comes from, provisioning
// one when cert-manager can issue it.
//
// The mode decides only what happens when no certificate can be had: `auto`
// accepts plaintext, `required` refuses. Neither ever downgrades a database
// that already has a certificate.
func (r *DatabaseReconciler) resolveInternalTLS(
	ctx context.Context,
	database *radv1alpha1.Database,
) (internalTransport, error) {
	mode := internalTLSMode(database)
	if !relayEnabled(database) || mode == radv1alpha1.InternalTLSDisabled {
		return internalTransport{source: certificateAbsent, ready: true}, nil
	}

	if name := database.Spec.InternalTLS.SecretName; name != "" {
		if err := r.validateSuppliedCertificate(ctx, database.Namespace, name); err != nil {
			return internalTransport{
				source:  certificateUserSupplied,
				secret:  name,
				pending: err.Error(),
			}, nil
		}
		return internalTransport{source: certificateUserSupplied, secret: name, ready: true}, nil
	}

	if r.CertManager == nil || !r.CertManager.Available(ctx) {
		if mode == radv1alpha1.InternalTLSRequired {
			return internalTransport{
				pending: "cert-manager is not installed and no certificate was supplied",
			}, nil
		}
		return internalTransport{source: certificateAbsent, ready: true}, nil
	}

	if err := r.reconcileCertificateChain(ctx, database); err != nil {
		return internalTransport{}, err
	}
	secret := internalCertificateSecretName(database.Name)
	ready, reason := r.certificateIssued(ctx, database)
	return internalTransport{
		source:  certificateManaged,
		secret:  secret,
		ready:   ready,
		pending: reason,
	}, nil
}

// A supplied Secret is validated and never touched. Its keys are the ones
// cert-manager writes, so the two sources are interchangeable to the workload.
func (r *DatabaseReconciler) validateSuppliedCertificate(
	ctx context.Context,
	namespace, name string,
) error {
	secret := &corev1.Secret{}
	if err := r.readClient().Get(ctx, types.NamespacedName{Namespace: namespace, Name: name}, secret); err != nil {
		if apierrors.IsNotFound(err) {
			return fmt.Errorf("TLS Secret %q does not exist", name)
		}
		return fmt.Errorf("TLS Secret %q cannot be read: %w", name, err)
	}
	for _, key := range []string{"tls.crt", "tls.key", "ca.crt"} {
		if len(secret.Data[key]) == 0 {
			return fmt.Errorf("TLS Secret %q is missing %s", name, key)
		}
	}
	return nil
}

func internalCertificateSecretName(databaseName string) string {
	return suffixedResourceName(databaseName, "-internal-tls")
}

func internalAuthorityName(databaseName string) string {
	return suffixedResourceName(databaseName, "-internal-ca")
}

func internalSelfSignedName(databaseName string) string {
	return suffixedResourceName(databaseName, "-internal-selfsigned")
}

// The writer Service names a reader may use. A certificate valid for all four
// means a reader can be configured with any of them and, more importantly,
// that writer election later changes the Service target rather than the
// certificate.
func writerCertificateNames(database *radv1alpha1.Database) []any {
	service := relayResourceName(database.Name)
	return []any{
		service,
		fmt.Sprintf("%s.%s", service, database.Namespace),
		fmt.Sprintf("%s.%s.svc", service, database.Namespace),
		fmt.Sprintf("%s.%s.svc.cluster.local", service, database.Namespace),
	}
}

// reconcileCertificateChain builds a certificate authority scoped to this one
// database, then a writer certificate from it.
//
// Namespace-scoped and per-database on purpose: a cluster-wide authority would
// let any database's certificate authenticate as any other. A deployment with
// its own PKI supplies issuerRef instead and no authority is created.
func (r *DatabaseReconciler) reconcileCertificateChain(
	ctx context.Context,
	database *radv1alpha1.Database,
) error {
	issuer := database.Spec.InternalTLS.IssuerRef
	if issuer == nil {
		if err := r.reconcileSelfSignedIssuer(ctx, database); err != nil {
			return err
		}
		if err := r.reconcileAuthorityCertificate(ctx, database); err != nil {
			return err
		}
		if err := r.reconcileAuthorityIssuer(ctx, database); err != nil {
			return err
		}
		issuer = &radv1alpha1.IssuerReference{
			Name:  internalAuthorityName(database.Name),
			Kind:  "Issuer",
			Group: certManagerGroup,
		}
	}
	return r.reconcileWriterCertificate(ctx, database, issuer)
}

func (r *DatabaseReconciler) reconcileSelfSignedIssuer(
	ctx context.Context,
	database *radv1alpha1.Database,
) error {
	return r.applyCertManagerObject(ctx, database, "Issuer", internalSelfSignedName(database.Name),
		map[string]any{"selfSigned": map[string]any{}})
}

func (r *DatabaseReconciler) reconcileAuthorityCertificate(
	ctx context.Context,
	database *radv1alpha1.Database,
) error {
	return r.applyCertManagerObject(ctx, database, "Certificate", internalAuthorityName(database.Name),
		map[string]any{
			"isCA":        true,
			"commonName":  internalAuthorityName(database.Name),
			"secretName":  internalAuthorityName(database.Name),
			"duration":    authorityDuration,
			"renewBefore": authorityRenewBefore,
			"privateKey":  map[string]any{"algorithm": "ECDSA", "size": int64(256)},
			"issuerRef": map[string]any{
				"name":  internalSelfSignedName(database.Name),
				"kind":  "Issuer",
				"group": certManagerGroup,
			},
		})
}

func (r *DatabaseReconciler) reconcileAuthorityIssuer(
	ctx context.Context,
	database *radv1alpha1.Database,
) error {
	return r.applyCertManagerObject(ctx, database, "Issuer", internalAuthorityName(database.Name),
		map[string]any{
			"ca": map[string]any{"secretName": internalAuthorityName(database.Name)},
		})
}

func (r *DatabaseReconciler) reconcileWriterCertificate(
	ctx context.Context,
	database *radv1alpha1.Database,
	issuer *radv1alpha1.IssuerReference,
) error {
	kind := issuer.Kind
	if kind == "" {
		kind = "Issuer"
	}
	group := issuer.Group
	if group == "" {
		group = certManagerGroup
	}
	return r.applyCertManagerObject(ctx, database, "Certificate", relayResourceName(database.Name),
		map[string]any{
			"secretName":  internalCertificateSecretName(database.Name),
			"dnsNames":    writerCertificateNames(database),
			"duration":    writerDuration,
			"renewBefore": writerRenewBefore,
			"privateKey":  map[string]any{"algorithm": "ECDSA", "size": int64(256)},
			"usages":      []any{"server auth"},
			"issuerRef": map[string]any{
				"name":  issuer.Name,
				"kind":  kind,
				"group": group,
			},
		})
}

// cert-manager's types are not a Go dependency of this operator. Four resource
// shapes do not justify the module, and unstructured objects keep the operator
// buildable in a cluster that has never heard of cert-manager.
func (r *DatabaseReconciler) applyCertManagerObject(
	ctx context.Context,
	database *radv1alpha1.Database,
	kind, name string,
	spec map[string]any,
) error {
	object := &unstructured.Unstructured{}
	object.SetGroupVersionKind(certManagerKind(kind))
	object.SetName(name)
	object.SetNamespace(database.Namespace)
	_, err := r.createOrUpdate(ctx, object, func() error {
		if err := r.prepareOwned(database, object); err != nil {
			return err
		}
		object.SetLabels(labelsFor(database))
		return unstructured.SetNestedMap(object.Object, spec, "spec")
	})
	return err
}

// certificateIssued reads the certificate's own Ready condition. The CRDs
// existing means it is valid to ask cert-manager for a certificate; only this
// says one was actually issued.
func (r *DatabaseReconciler) certificateIssued(
	ctx context.Context,
	database *radv1alpha1.Database,
) (bool, string) {
	object := &unstructured.Unstructured{}
	object.SetGroupVersionKind(certManagerKind("Certificate"))
	key := types.NamespacedName{Namespace: database.Namespace, Name: relayResourceName(database.Name)}
	if err := r.Get(ctx, key, object); err != nil {
		return false, "waiting for the writer certificate"
	}
	conditions, found, err := unstructured.NestedSlice(object.Object, "status", "conditions")
	if err != nil || !found {
		return false, "the writer certificate has not been processed yet"
	}
	for _, entry := range conditions {
		condition, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		if condition["type"] != "Ready" {
			continue
		}
		if condition["status"] == string(metav1.ConditionTrue) {
			return true, ""
		}
		if message, ok := condition["message"].(string); ok && message != "" {
			return false, message
		}
		return false, "the writer certificate is not ready"
	}
	return false, "the writer certificate has no readiness condition"
}

// certManagerObjects is what the operator owns when it provisions TLS itself.
// A user-supplied Secret is never in this list.
func certManagerObjects(database *radv1alpha1.Database) []client.Object {
	kinds := []struct {
		kind string
		name string
	}{
		{"Certificate", relayResourceName(database.Name)},
		{"Issuer", internalAuthorityName(database.Name)},
		{"Certificate", internalAuthorityName(database.Name)},
		{"Issuer", internalSelfSignedName(database.Name)},
	}
	objects := make([]client.Object, 0, len(kinds))
	for _, entry := range kinds {
		object := &unstructured.Unstructured{}
		object.SetGroupVersionKind(certManagerKind(entry.kind))
		object.SetName(entry.name)
		object.SetNamespace(database.Namespace)
		objects = append(objects, object)
	}
	return objects
}
