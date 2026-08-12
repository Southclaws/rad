package controller

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"net"
	"net/url"
	"strings"

	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"k8s.io/utils/ptr"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

const (
	claimKindLabel     = "radengine.dev/claim-kind"
	claimDatabaseUID   = "radengine.dev/database-uid"
	claimDatabaseIndex = "radengine.dev/claim-owner-uid"
	storageClaimKind   = "storage"
	routeClaimKind     = "route"
)

type durableClaim struct {
	kind           string
	key            string
	conflictReason string
}

func claimIndexValues(object client.Object) []string {
	uid := object.GetLabels()[claimDatabaseUID]
	if uid == "" {
		return nil
	}
	return []string{uid}
}

func (r *DatabaseReconciler) reconcileClaims(
	ctx context.Context,
	database *radv1alpha1.Database,
) (*dependencyProblem, error) {
	claims := []durableClaim{
		{kind: storageClaimKind, key: storageClaim(database), conflictReason: "BucketClaimConflict"},
		{kind: routeClaimKind, key: routeClaim(database), conflictReason: "HostnameClaimConflict"},
	}
	desiredNames := make(map[string]struct{}, len(claims))
	for _, claim := range claims {
		name := claimName(claim.kind, claim.key)
		desiredNames[name] = struct{}{}
		problem, err := r.acquireClaim(ctx, database, claim, name)
		if err != nil {
			return nil, err
		}
		if problem != nil {
			if cleanupErr := r.removeStaleClaims(ctx, database, desiredNames); cleanupErr != nil {
				return nil, cleanupErr
			}
			return problem, nil
		}
	}
	return nil, r.removeStaleClaims(ctx, database, desiredNames)
}

func (r *DatabaseReconciler) acquireClaim(
	ctx context.Context,
	database *radv1alpha1.Database,
	claim durableClaim,
	name string,
) (*dependencyProblem, error) {
	key := types.NamespacedName{Namespace: database.Namespace, Name: name}
	lease := &coordinationv1.Lease{}
	if err := r.readClient().Get(ctx, key, lease); err != nil {
		if !apierrors.IsNotFound(err) {
			return nil, err
		}
		holder := claimHolder(database)
		acquired := metav1.NowMicro()
		lease = &coordinationv1.Lease{
			ObjectMeta: metav1.ObjectMeta{
				Name:      name,
				Namespace: database.Namespace,
				Labels: map[string]string{
					"app.kubernetes.io/managed-by": "rad-operator",
					claimKindLabel:                 claim.kind,
					claimDatabaseUID:               string(database.UID),
				},
			},
			Spec: coordinationv1.LeaseSpec{
				HolderIdentity: &holder,
				AcquireTime:    &acquired,
			},
		}
		if err := r.prepareOwned(database, lease); err != nil {
			return nil, err
		}
		if err := r.Create(ctx, lease); err != nil {
			if !apierrors.IsAlreadyExists(err) {
				return nil, err
			}
			if err := r.readClient().Get(ctx, key, lease); err != nil {
				return nil, err
			}
		} else {
			r.event(database, corev1.EventTypeNormal, "ClaimAcquired", "%s claim %s acquired", claim.kind, name)
			return nil, nil
		}
	}

	holder := ptr.Deref(lease.Spec.HolderIdentity, "unknown")
	if metav1.IsControlledBy(lease, database) && holder == claimHolder(database) {
		return nil, nil
	}
	return &dependencyProblem{
		reason:  claim.conflictReason,
		message: fmt.Sprintf("%s claim is held by %s", claim.kind, holder),
	}, nil
}

func (r *DatabaseReconciler) removeStaleClaims(
	ctx context.Context,
	database *radv1alpha1.Database,
	desiredNames map[string]struct{},
) error {
	var leases coordinationv1.LeaseList
	options := []client.ListOption{client.InNamespace(database.Namespace)}
	if r.ClaimIndexAvailable {
		options = append(options, client.MatchingFields{claimDatabaseIndex: string(database.UID)})
	} else {
		options = append(options, client.MatchingLabels{claimDatabaseUID: string(database.UID)})
	}
	if err := r.List(ctx, &leases, options...); err != nil {
		return err
	}
	for index := range leases.Items {
		lease := &leases.Items[index]
		if _, desired := desiredNames[lease.Name]; desired {
			continue
		}
		if !metav1.IsControlledBy(lease, database) {
			return fmt.Errorf("refusing to remove unowned claim Lease %s/%s", lease.Namespace, lease.Name)
		}
		if err := r.Delete(ctx, lease); err != nil && !apierrors.IsNotFound(err) {
			return err
		}
	}
	return nil
}

func claimHolder(database *radv1alpha1.Database) string {
	return database.Namespace + "/" + database.Name + "/" + string(database.UID)
}

func claimName(kind, value string) string {
	digest := sha256.Sum256([]byte(value))
	return "rad-" + kind + "-" + hex.EncodeToString(digest[:16])
}

func storageClaim(database *radv1alpha1.Database) string {
	endpoint := canonicalEndpoint(database.Spec.Storage.Endpoint)
	return endpoint + "\x00" + strings.ToLower(database.Spec.Storage.Bucket)
}

func canonicalEndpoint(value string) string {
	value = strings.TrimSpace(value)
	parsed, err := url.Parse(value)
	if err != nil || parsed.Scheme == "" || parsed.Host == "" {
		return strings.TrimSuffix(strings.ToLower(value), "/")
	}
	parsed.Scheme = strings.ToLower(parsed.Scheme)
	hostname := strings.ToLower(parsed.Hostname())
	port := parsed.Port()
	if (parsed.Scheme == "http" && port == "80") || (parsed.Scheme == "https" && port == "443") {
		port = ""
	}
	if port != "" {
		parsed.Host = net.JoinHostPort(hostname, port)
	} else if strings.Contains(hostname, ":") {
		parsed.Host = "[" + hostname + "]"
	} else {
		parsed.Host = hostname
	}
	parsed.Path = strings.TrimSuffix(parsed.Path, "/")
	parsed.RawPath = ""
	parsed.RawQuery = ""
	parsed.Fragment = ""
	return parsed.String()
}

func routeClaim(database *radv1alpha1.Database) string {
	return strings.ToLower(strings.TrimSpace(database.Spec.Route.Hostname))
}
