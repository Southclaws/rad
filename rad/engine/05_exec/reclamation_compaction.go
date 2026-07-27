package exec

import (
	"context"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

func compactCompletedReclamation(
	ctx context.Context,
	view kv.KV,
	reclamation *model.Reclamation,
	completedAt time.Time,
) error {
	switch reclamation.Kind {
	case model.ReclamationTransitionDeltas, model.ReclamationCancelledIndex, model.ReclamationFailedIndex,
		model.ReclamationReplacedColumn, model.ReclamationCancelledReplacement,
		model.ReclamationFailedReplacement, model.ReclamationConstraintValidation:
		pinned, err := store.TransitionDiagnosticsPinned(ctx, view, reclamation.TransitionID)
		if err != nil {
			return err
		}
		if !pinned {
			if err := store.CompactTransition(ctx, view, reclamation.TransitionID, completedAt); err != nil {
				return err
			}
		}
	}
	reclamation.OwnerEpoch = 0
	reclamation.Phase = ""
	reclamation.Cursor = nil
	reclamation.BatchID = 0
	reclamation.IndexIDs = nil
	reclamation.LastError = ""
	reclamation.CompactedAt = completedAt.UTC()
	return nil
}
