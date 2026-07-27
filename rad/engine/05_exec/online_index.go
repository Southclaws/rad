package exec

import (
	"context"
	"errors"
	"fmt"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const defaultIndexBuildBatchSize = 128

// CancelSchemaTransition atomically invalidates worker ownership, removes any
// foreground obligation, and schedules partial physical state for reclamation.
func (e *Engine) CancelSchemaTransition(ctx context.Context, transitionID string) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := e.CatalogTxn(ctx, func(_ *Tx, mutation *change.Mutation) error {
		var err error
		transition, err = mutation.CancelSchemaTransition(ctx, transitionID)
		return err
	})
	if err == nil {
		transition.RefreshWorkState(transition.DeltaHighWater)
	}
	return transition, err
}

// inspectSchemaTransition returns current durable state plus derived delta
// backlog health without claiming or advancing the transition.
func (e *Engine) inspectSchemaTransition(ctx context.Context, transitionID string) (model.SchemaTransition, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Fail(reject.ReasonNotFound, "catalog: transition %q does not exist", transitionID)
	}
	transition.DeltaHighWater, err = store.DeltaHighWater(ctx, txn, transitionID)
	transition.RefreshWorkState(transition.DeltaHighWater)
	return transition, err
}

// claimIndexBuild advances the durable owner epoch. Any worker holding an
// older epoch may continue local computation but cannot commit its next batch.
func (e *Engine) claimIndexBuild(ctx context.Context, transitionID string) (uint64, error) {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return 0, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil {
		return 0, err
	}
	if !ok {
		return 0, reject.Inputf("catalog: transition %q does not exist", transitionID)
	}
	if transition.Kind != model.TransitionIndexBuild {
		return 0, reject.Inputf("catalog: transition %q is not an index build", transitionID)
	}
	switch transition.State {
	case model.TransitionBuilding, model.TransitionCatchingUp, model.TransitionValidating:
	case model.TransitionReady:
		return transition.OwnerEpoch, nil
	default:
		return 0, reject.Inputf("catalog: transition %q cannot be claimed in state %q", transitionID, transition.State)
	}
	transition.OwnerEpoch++
	transition.Generation++
	transition.LastError = ""
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, txn, transition); err != nil {
		return 0, err
	}
	if err := txn.Commit(ctx); err != nil {
		return 0, err
	}
	e.yield(ctx, YieldOwnerTakeover, transitionID)
	return transition.OwnerEpoch, nil
}

func (e *Engine) recordIndexBuildError(ctx context.Context, transitionID string, ownerEpoch uint64, cause error) error {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil || !ok {
		return err
	}
	if transition.OwnerEpoch != ownerEpoch || transition.State == model.TransitionReady ||
		transition.State == model.TransitionFailed || transition.State == model.TransitionCancelled {
		return nil
	}
	if transition.State == model.TransitionValidating && transition.Index.Unique {
		_, err := change.Apply(ctx, txn, func(mutation *change.Mutation) error {
			_, err := mutation.FailIndexValidation(ctx, transitionID, ownerEpoch, cause.Error())
			return err
		})
		if err != nil {
			return err
		}
		return txn.Commit(ctx)
	}
	transition.LastError = cause.Error()
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, txn, transition); err != nil {
		return err
	}
	return txn.Commit(ctx)
}

// runIndexBuild claims and drives a transition to ready. Each scan/delta batch
// is one durable transaction; the automatic scheduler may invoke
// stepIndexBuild directly to interleave bounded work fairly.
func (e *Engine) runIndexBuild(ctx context.Context, transitionID string, batchSize int) (model.SchemaTransition, error) {
	owner, err := e.claimIndexBuild(ctx, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if batchSize <= 0 {
		batchSize = defaultIndexBuildBatchSize
	}
	for {
		transition, err := e.stepIndexBuild(ctx, transitionID, owner, batchSize)
		if err != nil {
			if errors.Is(err, kv.ErrConflict) {
				current, inspectErr := e.inspectSchemaTransition(ctx, transitionID)
				if inspectErr != nil {
					return model.SchemaTransition{}, inspectErr
				}
				if current.OwnerEpoch != owner {
					return model.SchemaTransition{}, err
				}
				continue
			}
			return model.SchemaTransition{}, err
		}
		if transition.State == model.TransitionReady {
			return transition, nil
		}
		if transition.State == model.TransitionFailed {
			return transition, reject.Fail(reject.ReasonConstraintViolation, "%s", transition.LastError)
		}
	}
}

// stepIndexBuild commits at most one bounded scan/delta/finalization unit.
func (e *Engine) stepIndexBuild(ctx context.Context, transitionID string, ownerEpoch uint64, batchSize int) (model.SchemaTransition, error) {
	if batchSize <= 0 {
		batchSize = defaultIndexBuildBatchSize
	}
	current, err := e.inspectSchemaTransition(ctx, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if current.Kind != model.TransitionIndexBuild {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: transition %q is not an index build",
			transitionID,
		)
	}
	if current.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	if current.State == model.TransitionReady {
		return current, nil
	}
	if current.State == model.TransitionFailed {
		return current, nil
	}
	e.yield(ctx, YieldTransitionBatchIntent, transitionID)
	level := kv.Snapshot
	if current.State == model.TransitionValidating || current.Index.Unique {
		level = kv.SerializableSnapshot
	}
	txn, err := e.store.Begin(ctx, level)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q does not exist", transitionID)
	}
	if transition.Kind != model.TransitionIndexBuild {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: transition %q is not an index build",
			transitionID,
		)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	previousAppliedDelta := transition.AppliedDelta
	table, ok, err := store.New(txn).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q table no longer exists", transitionID)
	}

	switch transition.State {
	case model.TransitionBuilding:
		err = scanIndexBuildBatch(ctx, txn, table, &transition, batchSize)
	case model.TransitionCatchingUp:
		err = applyIndexDeltaBatch(ctx, txn, table, &transition, batchSize, false)
	case model.TransitionValidating:
		err = applyIndexDeltaBatch(ctx, txn, table, &transition, batchSize, true)
	default:
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q cannot run in state %q", transitionID, transition.State)
	}
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.AppliedDelta != previousAppliedDelta {
		if err := store.SaveDeltaApplied(ctx, txn, transitionID, transition.AppliedDelta); err != nil {
			return model.SchemaTransition{}, err
		}
	}
	highWater, err := store.DeltaHighWater(ctx, txn, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	transition.RefreshWorkState(highWater)
	if transition.State == model.TransitionReady {
		// PublishIndexReady saved the transition through change.Apply below.
	} else if err := store.SaveTransition(ctx, txn, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := txn.Commit(ctx); err != nil {
		return model.SchemaTransition{}, err
	}
	e.yield(ctx, YieldTransitionCheckpoint, transitionID)
	if current.State != model.TransitionValidating && transition.State == model.TransitionValidating && transition.Index.Unique {
		e.yield(ctx, YieldFinalizationGateAcquired, transitionID)
	}
	if transition.State == model.TransitionReady || transition.State == model.TransitionFailed ||
		transition.State == model.TransitionCancelled {
		e.kickSchemaJobs()
	}
	transition.DeltaHighWater, _ = store.DeltaHighWater(ctx, e.store, transitionID)
	transition.RefreshWorkState(transition.DeltaHighWater)
	return transition, nil
}

func scanIndexBuildBatch(ctx context.Context, txn kv.Txn, table model.Table, transition *model.SchemaTransition, batchSize int) error {
	rows, cursor, err := rowstore.ScanTableBatch(ctx, txn, table, transition.Cursor, batchSize)
	if err != nil {
		return err
	}
	for _, row := range rows {
		columns := table.IndexColumnNames(transition.Index)
		tuple, err := codec.EncodeRowTuple(row.Row, columns)
		if err != nil {
			return err
		}
		if transition.Index.Unique && !rowHasNull(row.Row, columns) {
			if err := store.PutUniqueClaim(ctx, txn, transition.ID, tuple, row.PK); err != nil {
				return err
			}
		}
		if err := txn.Put(ctx, codec.IndexKey(table.ID, transition.Index.ID, tuple, row.PK), row.PK); err != nil {
			return err
		}
	}
	transition.BatchID++
	transition.Generation++
	transition.RowsScanned += uint64(len(rows))
	transition.UpdatedAt = time.Now().UTC()
	if len(rows) == 0 {
		transition.State = model.TransitionCatchingUp
		transition.Index.State = model.IndexCatchingUp
		return nil
	}
	transition.Cursor = cursor
	return nil
}

func applyIndexDeltaBatch(ctx context.Context, txn kv.Txn, table model.Table, transition *model.SchemaTransition, batchSize int, finalizing bool) error {
	_, end := store.DeltaRange(transition.ID)
	start := store.DeltaKey(transition.ID, transition.AppliedDelta+1)
	it, err := txn.Scan(ctx, start, end)
	if err != nil {
		return err
	}
	defer it.Close()
	applied := 0
	for applied < batchSize && it.Next() {
		delta, err := store.DecodeIndexDelta(transition.ID, it.Key(), it.Value())
		if err != nil {
			return fmt.Errorf("catalog: transition %q has corrupt delta %q: %w", transition.ID, it.Key(), err)
		}
		key := codec.IndexKey(table.ID, transition.Index.ID, delta.Tuple, delta.PK)
		switch delta.Operation {
		case model.IndexDeltaPut:
			err = txn.Put(ctx, key, delta.PK)
		case model.IndexDeltaDelete:
			err = txn.Delete(ctx, key)
		default:
			err = fmt.Errorf("catalog: transition %q has unknown delta operation %q", transition.ID, delta.Operation)
		}
		if err != nil {
			return err
		}
		transition.AppliedDelta = delta.Sequence
		applied++
	}
	if err := it.Err(); err != nil {
		return err
	}
	transition.BatchID++
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if applied == batchSize {
		return nil
	}
	if !finalizing {
		if transition.Index.Unique {
			// Persist the final catch-up checkpoint before the catalog mutation
			// reloads the transition and installs the affected-table gate.
			if err := store.SaveTransition(ctx, txn, *transition); err != nil {
				return err
			}
			_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
				validated, err := mutation.BeginIndexValidation(ctx, transition.ID, transition.OwnerEpoch)
				if err == nil {
					*transition = validated
				}
				return err
			})
			return err
		}
		transition.State = model.TransitionValidating
		transition.Index.State = model.IndexValidating
		return nil
	}
	if transition.Index.Unique {
		duplicateTuple, duplicate, err := store.FirstUniqueViolation(ctx, txn, transition.ID)
		if err != nil {
			return err
		}
		if duplicate {
			cause := fmt.Sprintf("exec: cannot publish unique index %q: indexed tuple %x is shared by multiple rows",
				transition.Index.Name, duplicateTuple)
			_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
				failed, err := mutation.FailIndexValidation(ctx, transition.ID, transition.OwnerEpoch, cause)
				if err == nil {
					*transition = failed
				}
				return err
			})
			return err
		}
	}
	if positioned, ok := txn.(kv.PositionedTxn); ok {
		transition.BarrierPosition = model.DataPosition(positioned.BeginPosition())
	}
	// PublishIndexReady reloads the transition through the mutation service, so
	// persist this final delta checkpoint in the same transaction first.
	if err := store.SaveTransition(ctx, txn, *transition); err != nil {
		return err
	}
	_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
		published, err := mutation.PublishIndexReady(ctx, transition.ID, transition.OwnerEpoch)
		if err == nil {
			*transition = published
		}
		return err
	})
	return err
}

func rowHasNull(row lir.Row, columns []string) bool {
	for _, column := range columns {
		if row[column].Null {
			return true
		}
	}
	return false
}
