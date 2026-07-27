package store

import (
	"context"
	"fmt"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// CompactTransition discards worker-only state after physical cleanup while
// preserving the stable control result and terminal diagnostics at the same
// transition identity.
func CompactTransition(ctx context.Context, view kv.KV, id string, completedAt time.Time) error {
	transition, ok, err := GetTransition(ctx, view, id)
	if err != nil {
		return err
	}
	if !ok {
		return fmt.Errorf("catalog: transition %q does not exist", id)
	}
	switch transition.State {
	case model.TransitionReady, model.TransitionCancelled, model.TransitionFailed:
	default:
		return fmt.Errorf("catalog: transition %q is not terminal", id)
	}
	if !transition.CompactedAt.IsZero() {
		return nil
	}
	transition.Index = model.Index{
		ID:                   transition.Index.ID,
		LogicalID:            transition.Index.LogicalID,
		DefinitionGeneration: transition.Index.DefinitionGeneration,
		State:                transition.Index.State,
		Name:                 transition.Index.Name,
		Unique:               transition.Index.Unique,
	}
	transition.OwnerEpoch = 0
	transition.BasePosition = ""
	transition.BarrierPosition = ""
	transition.TableID = ""
	transition.Cursor = nil
	transition.BatchID = 0
	transition.AppliedDelta = 0
	transition.DeltaHighWater = 0
	transition.DeltaSoftLimit = 0
	transition.DeltaHardLimit = 0
	transition.WorkState = ""
	transition.RowsScanned = 0
	transition.AffectedColumnIDs = nil
	transition.IndexRequest = nil
	transition.ColumnReplacement = nil
	transition.ReplacementRequest = nil
	transition.Constraint = nil
	transition.ConstraintRequest = nil
	transition.Prerequisites = nil
	transition.GateTableIDs = nil
	transition.CompactedAt = completedAt.UTC()
	return SaveTransition(ctx, view, transition)
}
