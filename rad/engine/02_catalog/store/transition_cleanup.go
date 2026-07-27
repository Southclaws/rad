package store

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// TransitionCompactionEligible separates terminal diagnostic retention from
// physical cleanup. A detailed terminal record can be compacted only after its
// typed cleanup checkpoint is durable and no diagnostic pin retains it.
func TransitionCompactionEligible(
	ctx context.Context,
	view kv.KV,
	transition model.SchemaTransition,
) (bool, error) {
	if !transition.CompactedAt.IsZero() {
		return false, nil
	}
	if transition.Kind == model.TransitionIndexBuild &&
		transition.IndexRequest != nil &&
		len(transition.Index.ColumnIDs) == 0 {
		switch transition.State {
		case model.TransitionReady, model.TransitionCancelled, model.TransitionFailed:
			pinned, err := TransitionDiagnosticsPinned(ctx, view, transition.ID)
			return !pinned, err
		}
	}
	if transition.Kind == model.TransitionColumnReplacement &&
		transition.ColumnReplacement == nil {
		switch transition.State {
		case model.TransitionReady, model.TransitionCancelled, model.TransitionFailed:
			pinned, err := TransitionDiagnosticsPinned(ctx, view, transition.ID)
			return !pinned, err
		}
	}
	var reclamationID string
	switch transition.State {
	case model.TransitionReady:
		switch transition.Kind {
		case model.TransitionIndexBuild:
			reclamationID = TransitionDeltaReclamationID(transition.ID)
		case model.TransitionColumnReplacement:
			reclamationID = ReplacedColumnReclamationID(transition.ID)
		case model.TransitionConstraintValidation:
			reclamationID = ConstraintValidationReclamationID(transition.ID)
		}
	case model.TransitionCancelled:
		switch transition.Kind {
		case model.TransitionIndexBuild:
			reclamationID = CancelledIndexReclamationID(transition.ID)
		case model.TransitionColumnReplacement:
			reclamationID = CancelledReplacementReclamationID(transition.ID)
		case model.TransitionConstraintValidation:
			reclamationID = ConstraintValidationReclamationID(transition.ID)
		}
	case model.TransitionFailed:
		switch transition.Kind {
		case model.TransitionIndexBuild:
			reclamationID = FailedIndexReclamationID(transition.ID)
		case model.TransitionColumnReplacement:
			reclamationID = FailedReplacementReclamationID(transition.ID)
		case model.TransitionConstraintValidation:
			reclamationID = ConstraintValidationReclamationID(transition.ID)
		}
	default:
		return false, nil
	}
	if reclamationID == "" {
		return false, nil
	}
	pinned, err := TransitionDiagnosticsPinned(ctx, view, transition.ID)
	if err != nil || pinned {
		return false, err
	}
	reclamation, ok, err := GetReclamation(ctx, view, reclamationID)
	if err != nil || !ok {
		return false, err
	}
	return reclamation.State == model.ReclamationReclaimed, nil
}
