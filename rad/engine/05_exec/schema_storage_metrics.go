package exec

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

// schemaStorageMetrics contains one coherent Slate snapshot of durable
// schema-work and retention pressure, followed by a process-local scheduler
// counter sample. It is an engine diagnostic type, not a wire or persisted
// representation.
type schemaStorageMetrics struct {
	RetentionHorizons               model.RetentionHorizons
	CanonicalCatalogRevisions       uint64
	CatalogRevisionCompactedThrough uint64
	ActiveTransitions               uint64
	WaitingTransitions              uint64
	FailedTransitions               uint64
	UncompactedTerminalTransitions  uint64
	TransitionDeltaLag              uint64
	PendingReclamations             uint64
	FailedReclamations              uint64
	PinnedReclamations              uint64
	TerminalReclamationRecords      uint64
	ReclaimedItems                  uint64
	RunnerBatches                   uint64
	RunnerItems                     uint64
	RunnerBackoffs                  uint64
}

// schemaStorageMetrics samples durable schema-work pressure and process-local
// scheduler counters.
func (e *Engine) schemaStorageMetrics(ctx context.Context) (schemaStorageMetrics, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return schemaStorageMetrics{}, err
	}
	defer txn.Rollback()

	horizons, err := store.RetentionHorizons(ctx, txn)
	if err != nil {
		return schemaStorageMetrics{}, err
	}
	revisions, err := store.Revisions(ctx, txn)
	if err != nil {
		return schemaStorageMetrics{}, err
	}
	compactedThrough, err := store.RevisionCompactedThrough(ctx, txn)
	if err != nil {
		return schemaStorageMetrics{}, err
	}
	transitions, err := store.ListTransitions(ctx, txn)
	if err != nil {
		return schemaStorageMetrics{}, err
	}
	reclamations, err := store.ListReclamations(ctx, txn)
	if err != nil {
		return schemaStorageMetrics{}, err
	}

	metrics := schemaStorageMetrics{
		RetentionHorizons:               horizons,
		CanonicalCatalogRevisions:       uint64(len(revisions)),
		CatalogRevisionCompactedThrough: compactedThrough,
	}
	for _, transition := range transitions {
		lag := uint64(0)
		if transition.DeltaHighWater > transition.AppliedDelta {
			lag = transition.DeltaHighWater - transition.AppliedDelta
		}
		metrics.TransitionDeltaLag += lag
		switch transition.State {
		case model.TransitionWaiting:
			metrics.ActiveTransitions++
			metrics.WaitingTransitions++
		case model.TransitionBuilding, model.TransitionCatchingUp, model.TransitionValidating:
			metrics.ActiveTransitions++
		case model.TransitionFailed:
			metrics.FailedTransitions++
		}
		switch transition.State {
		case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
			if transition.CompactedAt.IsZero() {
				metrics.UncompactedTerminalTransitions++
			}
		}
	}
	for _, reclamation := range reclamations {
		metrics.ReclaimedItems += reclamation.ItemsReclaimed
		switch reclamation.State {
		case model.ReclamationPending, model.ReclamationReclaiming:
			metrics.PendingReclamations++
			_, pinned, err := store.RetentionBlocker(ctx, txn, reclamation)
			if err != nil {
				return schemaStorageMetrics{}, err
			}
			if pinned {
				metrics.PinnedReclamations++
			}
		case model.ReclamationFailed:
			metrics.FailedReclamations++
		case model.ReclamationReclaimed:
			metrics.TerminalReclamationRecords++
		}
	}
	if e.schemaJobs != nil {
		counters := e.schemaJobs.readCounters()
		metrics.RunnerBatches = counters.Batches
		metrics.RunnerItems = counters.Items
		metrics.RunnerBackoffs = counters.Backoffs
	}
	return metrics, nil
}
