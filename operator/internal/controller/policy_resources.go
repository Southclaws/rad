package controller

import (
	"context"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/util/intstr"
	"k8s.io/utils/ptr"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func (r *DatabaseReconciler) reconcilePodDisruptionBudget(ctx context.Context, database *radv1alpha1.Database) error {
	budget := &policyv1.PodDisruptionBudget{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, budget, func() error {
		if err := r.prepareOwned(database, budget); err != nil {
			return err
		}
		budget.Labels = roleLabelsFor(database, writeRole)
		budget.Spec = policyv1.PodDisruptionBudgetSpec{
			MinAvailable: ptr.To(intstr.FromInt32(1)),
			Selector:     &metav1.LabelSelector{MatchLabels: roleSelectorLabelsFor(database, writeRole)},
		}
		return nil
	})
	return err
}

func (r *DatabaseReconciler) reconcileNetworkPolicy(ctx context.Context, database *radv1alpha1.Database) error {
	policy := &networkingv1.NetworkPolicy{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}}
	if r.GatewayNamespace == "" {
		return r.deleteOwnedIfPresent(ctx, database, policy)
	}
	_, err := r.createOrUpdate(ctx, policy, func() error {
		if err := r.prepareOwned(database, policy); err != nil {
			return err
		}
		policy.Labels = labelsFor(database)
		peer := networkingv1.NetworkPolicyPeer{
			NamespaceSelector: &metav1.LabelSelector{MatchLabels: map[string]string{
				"kubernetes.io/metadata.name": r.GatewayNamespace,
			}},
		}
		if r.GatewayPodSelector != nil {
			peer.PodSelector = r.GatewayPodSelector.DeepCopy()
		}
		rules := []networkingv1.NetworkPolicyIngressRule{{
			From: []networkingv1.NetworkPolicyPeer{peer},
			Ports: []networkingv1.NetworkPolicyPort{{
				Protocol: ptr.To(corev1.ProtocolTCP),
				Port:     ptr.To(intstr.FromInt32(publicPort)),
			}},
		}}
		if relayEnabled(database) {
			// The internal port is reachable only from this database's own
			// readers. The gateway rule above deliberately does not cover it.
			rules = append(rules, networkingv1.NetworkPolicyIngressRule{
				From: []networkingv1.NetworkPolicyPeer{{
					PodSelector: &metav1.LabelSelector{
						MatchLabels: roleSelectorLabelsFor(database, readRole),
					},
				}},
				Ports: []networkingv1.NetworkPolicyPort{{
					Protocol: ptr.To(corev1.ProtocolTCP),
					Port:     ptr.To(intstr.FromInt32(internalPort)),
				}},
			})
		}
		policy.Spec = networkingv1.NetworkPolicySpec{
			PodSelector: metav1.LabelSelector{MatchLabels: selectorLabelsFor(database)},
			PolicyTypes: []networkingv1.PolicyType{networkingv1.PolicyTypeIngress},
			Ingress:     rules,
		}
		return nil
	})
	return err
}

func (r *DatabaseReconciler) quiesceRejectedDatabase(ctx context.Context, database *radv1alpha1.Database) error {
	objects := []client.Object{
		&networkingv1.Ingress{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}},
		&appsv1.StatefulSet{ObjectMeta: metav1.ObjectMeta{Name: resourceName(database.Name), Namespace: database.Namespace}},
		&appsv1.Deployment{ObjectMeta: metav1.ObjectMeta{Name: readerResourceName(database.Name), Namespace: database.Namespace}},
	}
	for _, object := range objects {
		if err := r.deleteOwnedIfPresent(ctx, database, object); err != nil {
			return err
		}
	}
	return nil
}

func (r *DatabaseReconciler) deleteOwnedIfPresent(ctx context.Context, database *radv1alpha1.Database, object client.Object) error {
	key := client.ObjectKeyFromObject(object)
	if err := r.Get(ctx, key, object); err != nil {
		return client.IgnoreNotFound(err)
	}
	if !metav1.IsControlledBy(object, database) {
		return fmt.Errorf("refusing to delete unowned %T %s", object, key)
	}
	return client.IgnoreNotFound(r.Delete(ctx, object))
}
