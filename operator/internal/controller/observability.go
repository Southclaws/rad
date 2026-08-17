package controller

import (
	"strings"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"sigs.k8s.io/controller-runtime/pkg/metrics"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

var (
	databaseReadyGauge = prometheus.NewGaugeVec(prometheus.GaugeOpts{
		Namespace: "rad",
		Subsystem: "operator",
		Name:      "database_ready",
		Help:      "Whether a Database is currently Ready.",
	}, []string{"namespace", "database"})
	claimConflictCounter = prometheus.NewCounterVec(prometheus.CounterOpts{
		Namespace: "rad",
		Subsystem: "operator",
		Name:      "claim_conflicts_total",
		Help:      "Number of durable storage or route claim conflicts.",
	}, []string{"kind"})
	reconcileErrorCounter = prometheus.NewCounter(prometheus.CounterOpts{
		Namespace: "rad",
		Subsystem: "operator",
		Name:      "reconcile_errors_total",
		Help:      "Number of Database reconciliations returning an error.",
	})
	timeToReadyHistogram = prometheus.NewHistogram(prometheus.HistogramOpts{
		Namespace: "rad",
		Subsystem: "operator",
		Name:      "time_to_ready_seconds",
		Help:      "Seconds from Database creation to its first Ready condition.",
		Buckets:   prometheus.ExponentialBuckets(1, 2, 12),
	})
)

func init() {
	metrics.Registry.MustRegister(
		databaseReadyGauge,
		claimConflictCounter,
		reconcileErrorCounter,
		timeToReadyHistogram,
	)
}

func (r *DatabaseReconciler) event(database *radv1alpha1.Database, eventType, reason, message string, args ...any) {
	if r.Recorder == nil {
		return
	}
	r.Recorder.Eventf(database, eventType, reason, message, args...)
}

// Observe creation-to-Ready once per database per controller instance. A
// steadily ready database never re-transitions after a controller restart, so
// it is not re-observed; only a database that becomes ready again after losing
// readiness across a restart can contribute a second, inflated sample.
func (r *DatabaseReconciler) observeTimeToReady(database *radv1alpha1.Database) {
	if _, seen := r.readyObserved.LoadOrStore(database.UID, struct{}{}); seen {
		return
	}
	timeToReadyHistogram.Observe(time.Since(database.CreationTimestamp.Time).Seconds())
}

func (r *DatabaseReconciler) setDependencyCondition(
	database *radv1alpha1.Database,
	conditionType string,
	reason string,
	message string,
) {
	changed := setCondition(database, conditionType, metav1.ConditionFalse, reason, message)
	setCondition(database, radv1alpha1.ConditionReady, metav1.ConditionFalse, reason, message)
	markDownstreamUnknown(database, conditionType, reason)
	databaseReadyGauge.WithLabelValues(database.Namespace, database.Name).Set(0)
	if !changed {
		return
	}
	if strings.HasSuffix(reason, "ClaimConflict") {
		kind := "route"
		if reason == "BucketClaimConflict" {
			kind = "storage"
		}
		claimConflictCounter.WithLabelValues(kind).Inc()
	}
	r.event(database, corev1.EventTypeWarning, reason, "%s", message)
}

// Conditions the reconcile establishes in order. When one fails, the
// reconcile did not evaluate the ones after it, so they must not keep
// asserting a state the controller no longer knows.
var conditionOrder = []string{
	radv1alpha1.ConditionClaimsAccepted,
	radv1alpha1.ConditionCredentialsReady,
	radv1alpha1.ConditionWorkloadReady,
	radv1alpha1.ConditionRouteReady,
}

func markDownstreamUnknown(database *radv1alpha1.Database, failed string, reason string) {
	seen := false
	for _, conditionType := range conditionOrder {
		if conditionType == failed {
			seen = true
			continue
		}
		if seen {
			setCondition(database, conditionType, metav1.ConditionUnknown, reason,
				"not evaluated: "+failed+" is not satisfied")
		}
	}
}

func setCondition(database *radv1alpha1.Database, conditionType string, status metav1.ConditionStatus, reason, message string) bool {
	current := meta.FindStatusCondition(database.Status.Conditions, conditionType)
	changed := current == nil || current.Status != status || current.Reason != reason || current.Message != message
	meta.SetStatusCondition(&database.Status.Conditions, metav1.Condition{
		Type:               conditionType,
		Status:             status,
		ObservedGeneration: database.Generation,
		Reason:             reason,
		Message:            message,
	})
	return changed
}
