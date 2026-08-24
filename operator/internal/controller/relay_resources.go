package controller

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"fmt"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/utils/ptr"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

const (
	relayTokenKey   = "relay-token"
	relayTokenBytes = 32
	relayMountPath  = "/etc/rad/relay"
	// The writer's certificate and the authority a reader verifies against
	// come from one Secret, mounted at a different path per role.
	relayCertificateMountPath = "/etc/rad/relay-tls"
	relayAuthorityMountPath   = "/etc/rad/relay-ca"
)

func relayCertificatePath() string {
	return relayCertificateMountPath + "/tls.crt"
}

func relayCertificateKeyPath() string {
	return relayCertificateMountPath + "/tls.key"
}

func relayAuthorityPath() string {
	return relayAuthorityMountPath + "/ca.crt"
}

func relayTokenPath() string {
	return relayMountPath + "/" + relayTokenKey
}

// The relay exists to carry reader evidence to the writer, so it is
// provisioned only when there are readers to carry it from.
func relayEnabled(database *radv1alpha1.Database) bool {
	return desiredReaders(database) > 0
}

func (r *DatabaseReconciler) reconcileRelay(ctx context.Context, database *radv1alpha1.Database) error {
	if !relayEnabled(database) {
		return r.removeRelay(ctx, database)
	}
	if err := r.reconcileRelaySecret(ctx, database); err != nil {
		return err
	}
	return r.reconcileWriterInternalService(ctx, database)
}

func (r *DatabaseReconciler) removeRelay(ctx context.Context, database *radv1alpha1.Database) error {
	objects := relayObjects(database)
	if r.CertManager != nil && r.CertManager.Available(ctx) {
		objects = append(objects, certManagerObjects(database)...)
	}
	for _, object := range objects {
		if err := r.deleteOwnedIfPresent(ctx, database, object); err != nil {
			return err
		}
	}
	return nil
}

func relayObjects(database *radv1alpha1.Database) []client.Object {
	meta := metav1.ObjectMeta{Name: relayResourceName(database.Name), Namespace: database.Namespace}
	return []client.Object{
		&corev1.Service{ObjectMeta: meta},
		&corev1.Secret{ObjectMeta: meta},
	}
}

// The secret is generated once and adopted afterwards. Regenerating it on
// every reconcile would break every reader on every loop, and rotating it is a
// deliberate act rather than a side effect of reconciliation.
func (r *DatabaseReconciler) reconcileRelaySecret(ctx context.Context, database *radv1alpha1.Database) error {
	key := types.NamespacedName{Namespace: database.Namespace, Name: relayResourceName(database.Name)}
	existing := &corev1.Secret{}
	err := r.readClient().Get(ctx, key, existing)
	if err == nil {
		if len(existing.Data[relayTokenKey]) > 0 {
			return nil
		}
	} else if !apierrors.IsNotFound(err) {
		return err
	}

	token := make([]byte, relayTokenBytes)
	if _, err := rand.Read(token); err != nil {
		return fmt.Errorf("generate relay token: %w", err)
	}
	secret := &corev1.Secret{ObjectMeta: metav1.ObjectMeta{Name: key.Name, Namespace: key.Namespace}}
	_, err = r.createOrUpdate(ctx, secret, func() error {
		if err := r.prepareOwned(database, secret); err != nil {
			return err
		}
		secret.Labels = labelsFor(database)
		secret.Type = corev1.SecretTypeOpaque
		if len(secret.Data[relayTokenKey]) > 0 {
			return nil
		}
		secret.Data = map[string][]byte{
			relayTokenKey: []byte(base64.RawURLEncoding.EncodeToString(token)),
		}
		return nil
	})
	return err
}

// Readers address the writer through a Service rather than a pod, so writer
// election later changes the Service target and no reader configuration.
func (r *DatabaseReconciler) reconcileWriterInternalService(ctx context.Context, database *radv1alpha1.Database) error {
	service := &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: relayResourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, service, func() error {
		if err := r.prepareOwned(database, service); err != nil {
			return err
		}
		service.Labels = roleLabelsFor(database, writeRole)
		service.Spec.Type = corev1.ServiceTypeClusterIP
		service.Spec.Selector = roleSelectorLabelsFor(database, writeRole)
		service.Spec.Ports = []corev1.ServicePort{{
			Name:       "internal",
			Port:       internalPort,
			TargetPort: intstrFromInt(internalPort),
			Protocol:   corev1.ProtocolTCP,
		}}
		return nil
	})
	return err
}

// The token is mounted as a file, never passed as an argument or an inline
// environment value: argv is readable by any process on the host, and
// environment blocks are inherited by children and captured in crash dumps.
func relayVolume(database *radv1alpha1.Database) corev1.Volume {
	return corev1.Volume{
		Name: "relay-token",
		VolumeSource: corev1.VolumeSource{
			Secret: &corev1.SecretVolumeSource{
				SecretName: relayResourceName(database.Name),
				// Readable by the pod's runtime group, which fsGroup makes the
				// owning group of the mount, and by nobody else.
				DefaultMode: ptr.To[int32](0o440),
			},
		},
	}
}

func relayVolumeMount() corev1.VolumeMount {
	return corev1.VolumeMount{
		Name:      "relay-token",
		MountPath: relayMountPath,
		ReadOnly:  true,
	}
}

// The writer serves the certificate; a reader verifies against the authority
// in the same Secret. Each projects only the keys its role needs, so a reader
// never holds the private key of the identity it exists to verify.
func relayTLSVolume(transport internalTransport, role string) corev1.Volume {
	if role == writeRole {
		return corev1.Volume{
			Name: "relay-tls",
			VolumeSource: corev1.VolumeSource{
				Secret: &corev1.SecretVolumeSource{
					SecretName:  transport.secret,
					DefaultMode: ptr.To[int32](0o440),
					Items: []corev1.KeyToPath{
						{Key: "tls.crt", Path: "tls.crt"},
						{Key: "tls.key", Path: "tls.key"},
					},
				},
			},
		}
	}
	return corev1.Volume{
		Name: "relay-ca",
		VolumeSource: corev1.VolumeSource{
			Secret: &corev1.SecretVolumeSource{
				SecretName:  transport.secret,
				DefaultMode: ptr.To[int32](0o440),
				Items:       []corev1.KeyToPath{{Key: "ca.crt", Path: "ca.crt"}},
			},
		},
	}
}

func relayTLSVolumeMount(role string) corev1.VolumeMount {
	if role == writeRole {
		return corev1.VolumeMount{
			Name:      "relay-tls",
			MountPath: relayCertificateMountPath,
			ReadOnly:  true,
		}
	}
	return corev1.VolumeMount{
		Name:      "relay-ca",
		MountPath: relayAuthorityMountPath,
		ReadOnly:  true,
	}
}
