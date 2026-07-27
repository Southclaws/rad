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

// claimConstraintValidation advances the transition's owner epoch and returns
// the epoch required by subsequent worker batches.
func (e *Engine) claimConstraintValidation(ctx context.Context, transitionID string) (uint64, error) {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return 0, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil {
		return 0, err
	}
	if !ok || transition.Kind != model.TransitionConstraintValidation {
		return 0, reject.Inputf("catalog: constraint validation transition %q does not exist", transitionID)
	}
	switch transition.State {
	case model.TransitionBuilding, model.TransitionValidating:
	case model.TransitionReady:
		return transition.OwnerEpoch, nil
	default:
		return 0, reject.Inputf(
			"catalog: constraint validation transition %q cannot be claimed in state %q",
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

// runConstraintValidation claims and drives a constraint transition to a
// terminal outcome using bounded batches.
func (e *Engine) runConstraintValidation(
	ctx context.Context,
	transitionID string,
	batchSize int,
) (model.SchemaTransition, error) {
	owner, err := e.claimConstraintValidation(ctx, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if batchSize <= 0 {
		batchSize = defaultIndexBuildBatchSize
	}
	for {
		transition, err := e.stepConstraintValidation(ctx, transitionID, owner, batchSize)
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

// stepConstraintValidation commits at most one historical-scan,
// finalization, or publication unit under the supplied owner epoch.
func (e *Engine) stepConstraintValidation(
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
	if current.Kind != model.TransitionConstraintValidation || current.Constraint == nil {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: transition %q is not a constraint validation",
			transitionID,
		)
	}
	if current.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf(
			"catalog: transition %q ownership changed: %w",
			transitionID,
			kv.ErrConflict,
		)
	}
	switch current.State {
	case model.TransitionReady, model.TransitionFailed:
		return current, nil
	}
	e.yield(ctx, YieldTransitionBatchIntent, transitionID)
	level := kv.Snapshot
	if current.State == model.TransitionValidating ||
		current.Constraint.State == model.ConstraintEnforcingNewWrites {
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
	if !ok || transition.Kind != model.TransitionConstraintValidation || transition.Constraint == nil {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint validation transition %q does not exist",
			transitionID,
		)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf(
			"catalog: transition %q ownership changed: %w",
			transitionID,
			kv.ErrConflict,
		)
	}
	table, ok, err := store.New(txn).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint transition %q table no longer exists",
			transitionID,
		)
	}

	switch {
	case transition.State == model.TransitionBuilding &&
		transition.Constraint.State == model.ConstraintEnforcingNewWrites:
		_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
			validating, err := mutation.BeginConstraintHistoricalValidation(
				ctx,
				transition.ID,
				transition.OwnerEpoch,
			)
			if err == nil {
				transition = validating
			}
			return err
		})
	case transition.State == model.TransitionBuilding:
		err = scanConstraintBatch(ctx, txn, table, &transition, batchSize)
	case transition.State == model.TransitionValidating:
		err = validateConstraint(ctx, txn, &transition)
	default:
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint transition %q cannot run in state %q",
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

func scanConstraintBatch(
	ctx context.Context,
	txn kv.Txn,
	table model.Table,
	transition *model.SchemaTransition,
	batchSize int,
) error {
	if transition.Constraint.Kind != model.ConstraintNotNull ||
		len(transition.Constraint.ColumnIDs) != 1 {
		return reject.Fail(
			reject.ReasonCatalogDrift,
			"exec: constraint %q has invalid not-null definition",
			transition.Constraint.Name,
		)
	}
	column, ok := physicalColumn(table, transition.Constraint.ColumnIDs[0])
	if !ok {
		return reject.Fail(
			reject.ReasonCatalogDrift,
			"exec: constraint %q physical column %q is missing",
			transition.Constraint.Name,
			transition.Constraint.ColumnIDs[0],
		)
	}
	rows, cursor, err := rowstore.ScanRawTableBatch(ctx, txn, table, transition.Cursor, batchSize)
	if err != nil {
		return err
	}
	for _, row := range rows {
		value, err := codec.ReadColumnValue(row.Raw, column)
		if err != nil {
			return err
		}
		if value.Null {
			if err := store.PutTransitionViolation(
				ctx,
				txn,
				transition.ID,
				row.PK,
				fmt.Sprintf("column %q is NULL", column.Name),
			); err != nil {
				return err
			}
			continue
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
			validating, err := mutation.BeginConstraintFinalization(
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

func validateConstraint(
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
			"exec: cannot validate constraint %q: row %x: %s",
			transition.Constraint.Name,
			rowIdentity,
			cause,
		)
		_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
			failed, err := mutation.FailConstraintValidation(
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
		ready, err := mutation.PublishConstraint(
			ctx,
			transition.ID,
			transition.OwnerEpoch,
		)
		if err == nil {
			*transition = ready
		}
		return err
	})
	return err
}

func physicalColumn(table model.Table, id string) (model.Column, bool) {
	for _, column := range table.Columns {
		if column.ID == id {
			return column, true
		}
	}
	return model.Column{}, false
}
