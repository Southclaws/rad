package controller

import (
	"context"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	policyv1 "k8s.io/api/policy/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/util/intstr"
	"k8s.io/utils/ptr"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func desiredReaders(database *radv1alpha1.Database) int32 {
	if database.Spec.Readers < 0 {
		return 0
	}
	return database.Spec.Readers
}

// Reader resources exist only while readers are requested. Scaling to zero
// removes them rather than leaving an empty Deployment behind, which keeps
// "no readers" indistinguishable from a database that never had any.
func (r *DatabaseReconciler) reconcileReaders(
	ctx context.Context,
	database *radv1alpha1.Database,
	authentication resolvedAuthentication,
	transport internalTransport,
) error {
	if desiredReaders(database) == 0 {
		return r.removeReaders(ctx, database)
	}
	if err := r.reconcileReaderService(ctx, database); err != nil {
		return err
	}
	if err := r.reconcileReaderDisruptionBudget(ctx, database); err != nil {
		return err
	}
	return r.reconcileReaderDeployment(ctx, database, authentication, transport)
}

func (r *DatabaseReconciler) removeReaders(ctx context.Context, database *radv1alpha1.Database) error {
	for _, object := range readerObjects(database) {
		if err := r.deleteOwnedIfPresent(ctx, database, object); err != nil {
			return err
		}
	}
	return nil
}

func readerObjects(database *radv1alpha1.Database) []client.Object {
	meta := metav1.ObjectMeta{Name: readerResourceName(database.Name), Namespace: database.Namespace}
	return []client.Object{
		&appsv1.Deployment{ObjectMeta: meta},
		&policyv1.PodDisruptionBudget{ObjectMeta: meta},
		&corev1.Service{ObjectMeta: meta},
	}
}

func (r *DatabaseReconciler) reconcileReaderService(ctx context.Context, database *radv1alpha1.Database) error {
	service := &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: readerResourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, service, func() error {
		if err := r.prepareOwned(database, service); err != nil {
			return err
		}
		service.Labels = roleLabelsFor(database, readRole)
		service.Spec.Type = corev1.ServiceTypeClusterIP
		service.Spec.Selector = roleSelectorLabelsFor(database, readRole)
		service.Spec.Ports = []corev1.ServicePort{{
			Name:        "http",
			Port:        80,
			TargetPort:  intstrFromInt(publicPort),
			Protocol:    corev1.ProtocolTCP,
			AppProtocol: ptr.To("http"),
		}}
		return nil
	})
	return err
}

// One reader at a time may be evicted voluntarily. A budget that protected
// every reader would block a node drain, and a reader holds nothing that a
// drain could lose.
func (r *DatabaseReconciler) reconcileReaderDisruptionBudget(ctx context.Context, database *radv1alpha1.Database) error {
	budget := &policyv1.PodDisruptionBudget{ObjectMeta: metav1.ObjectMeta{Name: readerResourceName(database.Name), Namespace: database.Namespace}}
	_, err := r.createOrUpdate(ctx, budget, func() error {
		if err := r.prepareOwned(database, budget); err != nil {
			return err
		}
		budget.Labels = roleLabelsFor(database, readRole)
		budget.Spec = policyv1.PodDisruptionBudgetSpec{
			MaxUnavailable: ptr.To(intstr.FromInt32(1)),
			Selector:       &metav1.LabelSelector{MatchLabels: roleSelectorLabelsFor(database, readRole)},
		}
		return nil
	})
	return err
}

func (r *DatabaseReconciler) reconcileReaderDeployment(
	ctx context.Context,
	database *radv1alpha1.Database,
	authentication resolvedAuthentication,
	transport internalTransport,
) error {
	deployment := &appsv1.Deployment{ObjectMeta: metav1.ObjectMeta{Name: readerResourceName(database.Name), Namespace: database.Namespace}}
	replicas := desiredReaders(database)
	operation, err := r.createOrUpdate(ctx, deployment, func() error {
		if err := r.prepareOwned(database, deployment); err != nil {
			return err
		}
		deployment.Labels = roleLabelsFor(database, readRole)
		deployment.Spec.Replicas = ptr.To(replicas)
		deployment.Spec.RevisionHistoryLimit = ptr.To[int32](3)
		deployment.Spec.MinReadySeconds = 5
		deployment.Spec.Selector = &metav1.LabelSelector{MatchLabels: roleSelectorLabelsFor(database, readRole)}
		// Surging before terminating keeps read capacity whole across a
		// rollout. Readers are interchangeable, so the extra pod costs nothing
		// but scheduling room.
		deployment.Spec.Strategy = appsv1.DeploymentStrategy{
			Type: appsv1.RollingUpdateDeploymentStrategyType,
			RollingUpdate: &appsv1.RollingUpdateDeployment{
				MaxSurge:       ptr.To(intstr.FromInt32(1)),
				MaxUnavailable: ptr.To(intstr.FromInt32(0)),
			},
		}
		deployment.Spec.Template = corev1.PodTemplateSpec{
			ObjectMeta: metav1.ObjectMeta{
				Labels:      roleLabelsFor(database, readRole),
				Annotations: workloadAnnotations(database, authentication),
			},
			Spec: r.radPodSpec(database, authentication, transport, readRole),
		}
		return nil
	})
	if err == nil && operation == controllerutil.OperationResultUpdated {
		r.event(database, corev1.EventTypeNormal, "ReadersUpdated", "Rad reader configuration updated")
	}
	return err
}

// Ready readers, and the Service that addresses them. A missing Deployment is
// not an error: readers are optional, and the count may have just reached zero.
func (r *DatabaseReconciler) observeReaders(ctx context.Context, database *radv1alpha1.Database) error {
	database.Status.DesiredReaders = desiredReaders(database)
	if database.Status.DesiredReaders == 0 {
		database.Status.ReaderServiceName = ""
		database.Status.ReadyReaders = 0
		return nil
	}
	database.Status.ReaderServiceName = readerResourceName(database.Name)
	deployment := &appsv1.Deployment{}
	key := client.ObjectKey{Namespace: database.Namespace, Name: readerResourceName(database.Name)}
	if err := r.Get(ctx, key, deployment); err != nil {
		database.Status.ReadyReaders = 0
		return client.IgnoreNotFound(err)
	}
	database.Status.ReadyReaders = deployment.Status.ReadyReplicas
	return nil
}
