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
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// claimColumnReplacement advances the transition's owner epoch and returns
// the epoch required by subsequent worker batches.
func (e *Engine) claimColumnReplacement(ctx context.Context, transitionID string) (uint64, error) {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return 0, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil {
		return 0, err
	}
	if !ok || transition.Kind != model.TransitionColumnReplacement {
		return 0, reject.Inputf("catalog: column replacement transition %q does not exist", transitionID)
	}
	switch transition.State {
	case model.TransitionBuilding, model.TransitionValidating:
	case model.TransitionReady:
		return transition.OwnerEpoch, nil
	default:
		return 0, reject.Inputf(
			"catalog: column replacement transition %q cannot be claimed in state %q",
			transitionID,
			transition.State,
		)
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

func (e *Engine) recordSchemaTransitionError(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
	cause error,
) error {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil || !ok {
		return err
	}
	if transition.OwnerEpoch != ownerEpoch {
		return nil
	}
	switch transition.State {
	case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
		return nil
	}
	transition.LastError = cause.Error()
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, txn, transition); err != nil {
		return err
	}
	return txn.Commit(ctx)
}

// runColumnReplacement claims and drives a replacement transition to a
// terminal outcome using bounded batches.
func (e *Engine) runColumnReplacement(
	ctx context.Context,
	transitionID string,
	batchSize int,
) (model.SchemaTransition, error) {
	owner, err := e.claimColumnReplacement(ctx, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if batchSize <= 0 {
		batchSize = defaultIndexBuildBatchSize
	}
	for {
		transition, err := e.stepColumnReplacement(ctx, transitionID, owner, batchSize)
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
		switch transition.State {
		case model.TransitionReady:
			return transition, nil
		case model.TransitionFailed:
			return transition, reject.Fail(reject.ReasonConstraintViolation, "%s", transition.LastError)
		}
	}
}

// stepColumnReplacement commits one bounded representation backfill or one
// short validation/publication transaction.
func (e *Engine) stepColumnReplacement(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
	batchSize int,
) (model.SchemaTransition, error) {
	if batchSize <= 0 {
		batchSize = defaultIndexBuildBatchSize
	}
	current, err := e.inspectSchemaTransition(ctx, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if current.Kind != model.TransitionColumnReplacement || current.ColumnReplacement == nil {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q is not a column replacement", transitionID)
	}
	if current.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	switch current.State {
	case model.TransitionReady, model.TransitionFailed:
		return current, nil
	}
	e.yield(ctx, YieldTransitionBatchIntent, transitionID)
	level := kv.Snapshot
	if current.State == model.TransitionValidating {
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
	if !ok || transition.Kind != model.TransitionColumnReplacement || transition.ColumnReplacement == nil {
		return model.SchemaTransition{}, reject.Inputf("catalog: column replacement transition %q does not exist", transitionID)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	table, ok, err := store.New(txn).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: replacement transition %q table no longer exists", transitionID)
	}

	switch transition.State {
	case model.TransitionBuilding:
		err = scanColumnReplacementBatch(ctx, txn, table, &transition, batchSize)
	case model.TransitionValidating:
		err = validateColumnReplacement(ctx, txn, &transition)
	default:
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: column replacement transition %q cannot run in state %q",
			transitionID,
			transition.State,
		)
	}
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.State != model.TransitionReady && transition.State != model.TransitionFailed {
		if err := store.SaveTransition(ctx, txn, transition); err != nil {
			return model.SchemaTransition{}, err
		}
	}
	if err := txn.Commit(ctx); err != nil {
		return model.SchemaTransition{}, err
	}
	e.yield(ctx, YieldTransitionCheckpoint, transitionID)
	if current.State != model.TransitionValidating && transition.State == model.TransitionValidating {
		e.yield(ctx, YieldFinalizationGateAcquired, transitionID)
	}
	if transition.State == model.TransitionReady || transition.State == model.TransitionFailed {
		e.kickSchemaJobs()
	}
	return transition, nil
}

func scanColumnReplacementBatch(
	ctx context.Context,
	txn kv.Txn,
	table model.Table,
	transition *model.SchemaTransition,
	batchSize int,
) error {
	rows, cursor, err := rowstore.ScanRawTableBatch(ctx, txn, table, transition.Cursor, batchSize)
	if err != nil {
		return err
	}
	replacement := *transition.ColumnReplacement
	for _, row := range rows {
		source, err := codec.ReadColumnValue(row.Raw, replacement.Source)
		if err != nil {
			return err
		}
		target, conversionErr := codec.ConvertColumnValue(source, replacement.Target, replacement.Conversion)
		if conversionErr != nil {
			if err := store.PutTransitionViolation(
				ctx,
				txn,
				transition.ID,
				row.PK,
				conversionErr.Error(),
			); err != nil {
				return err
			}
			continue
		}
		raw, err := codec.SetColumnValue(row.Raw, replacement.Target, target)
		if err != nil {
			return err
		}
		if err := txn.Put(ctx, row.Key, raw); err != nil {
			return err
		}
		if err := store.DeleteTransitionViolation(ctx, txn, transition.ID, row.PK); err != nil {
			return err
		}
	}
	transition.BatchID++
	transition.Generation++
	transition.RowsScanned += uint64(len(rows))
	transition.UpdatedAt = time.Now().UTC()
	if len(rows) == 0 {
		if err := store.SaveTransition(ctx, txn, *transition); err != nil {
			return err
		}
		_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
			validating, err := mutation.BeginColumnReplacementValidation(
				ctx,
				transition.ID,
				transition.OwnerEpoch,
			)
			if err == nil {
				*transition = validating
			}
			return err
		})
		return err
	}
	transition.Cursor = cursor
	return nil
}

func validateColumnReplacement(
	ctx context.Context,
	txn kv.Txn,
	transition *model.SchemaTransition,
) error {
	rowIdentity, cause, violation, err := store.FirstTransitionViolation(ctx, txn, transition.ID)
	if err != nil {
		return err
	}
	if err := store.SaveTransition(ctx, txn, *transition); err != nil {
		return err
	}
	if violation {
		message := fmt.Sprintf(
			"exec: cannot publish replacement for column %q: row %x: %s",
			transition.ColumnReplacement.Source.Name,
			rowIdentity,
			cause,
		)
		_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
			failed, err := mutation.FailColumnReplacement(
				ctx,
				transition.ID,
				transition.OwnerEpoch,
				message,
			)
			if err == nil {
				*transition = failed
			}
			return err
		})
		return err
	}
	if positioned, ok := txn.(kv.PositionedTxn); ok {
		transition.BarrierPosition = model.DataPosition(positioned.BeginPosition())
	}
	if err := store.SaveTransition(ctx, txn, *transition); err != nil {
		return err
	}
	_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
		ready, err := mutation.PublishColumnReplacement(ctx, transition.ID, transition.OwnerEpoch)
		if err == nil {
			*transition = ready
		}
		return err
	})
	return err
}
