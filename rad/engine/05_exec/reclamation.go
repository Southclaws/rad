package exec

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

const defaultReclamationBatchSize = 128

func (e *Engine) claimReclamation(ctx context.Context, id string) (uint64, bool, error) {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return 0, false, err
	}
	defer txn.Rollback()
	reclamation, ok, err := store.GetReclamation(ctx, txn, id)
	if err != nil || !ok {
		return 0, false, err
	}
	if reclamation.State == model.ReclamationReclaimed || reclamation.State == model.ReclamationFailed {
		return reclamation.OwnerEpoch, false, nil
	}
	reclamation.OwnerEpoch++
	reclamation.Generation++
	reclamation.State = model.ReclamationReclaiming
	reclamation.LastError = ""
	reclamation.UpdatedAt = time.Now().UTC()
	if err := store.SaveReclamation(ctx, txn, reclamation); err != nil {
		return 0, false, err
	}
	if err := txn.Commit(ctx); err != nil {
		return 0, false, err
	}
	return reclamation.OwnerEpoch, true, nil
}

func (e *Engine) runReclamation(ctx context.Context, id string, owner uint64, batchSize int) error {
	for {
		reclamation, err := e.stepReclamation(ctx, id, owner, batchSize)
		if err != nil {
			if !errors.Is(err, kv.ErrConflict) {
				return err
			}
			current, ok, inspectErr := store.GetReclamation(ctx, e.store, id)
			if inspectErr != nil {
				return inspectErr
			}
			if !ok || current.OwnerEpoch != owner {
				return err
			}
			continue
		}
		if reclamation.State == model.ReclamationReclaimed {
			return nil
		}
	}
}

func (e *Engine) stepReclamation(ctx context.Context, id string, owner uint64, batchSize int) (model.Reclamation, error) {
	if batchSize <= 0 {
		batchSize = defaultReclamationBatchSize
	}
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return model.Reclamation{}, err
	}
	defer txn.Rollback()
	reclamation, ok, err := store.GetReclamation(ctx, txn, id)
	if err != nil {
		return model.Reclamation{}, err
	}
	if !ok {
		return model.Reclamation{}, fmt.Errorf("catalog: reclamation %q does not exist", id)
	}
	if reclamation.OwnerEpoch != owner {
		return model.Reclamation{}, fmt.Errorf("catalog: reclamation %q ownership changed: %w", id, kv.ErrConflict)
	}
	if reclamation.State == model.ReclamationReclaimed {
		return reclamation, nil
	}
	if reclamation.State != model.ReclamationReclaiming {
		return model.Reclamation{}, fmt.Errorf("catalog: reclamation %q is in state %q", id, reclamation.State)
	}
	if pin, blocked, err := store.RetentionBlocker(ctx, txn, reclamation); err != nil {
		return model.Reclamation{}, err
	} else if blocked {
		return model.Reclamation{}, retentionPinnedError(pin, reclamation)
	}
	if err := validateReclamationEligibility(ctx, txn, reclamation); err != nil {
		return model.Reclamation{}, err
	}

	count, done, err := applyReclamationBatch(ctx, txn, &reclamation, batchSize)
	if err != nil {
		return model.Reclamation{}, err
	}
	reclamation.BatchID++
	reclamation.ItemsReclaimed += uint64(count)
	reclamation.Generation++
	reclamation.UpdatedAt = time.Now().UTC()
	if done {
		reclamation.State = model.ReclamationReclaimed
		reclamation.Cursor = nil
		if err := compactCompletedReclamation(ctx, txn, &reclamation, reclamation.UpdatedAt); err != nil {
			return model.Reclamation{}, err
		}
	}
	if err := store.SaveReclamation(ctx, txn, reclamation); err != nil {
		return model.Reclamation{}, err
	}
	if err := txn.Commit(ctx); err != nil {
		return model.Reclamation{}, err
	}
	return reclamation, nil
}

func validateReclamationEligibility(ctx context.Context, view kv.KV, reclamation model.Reclamation) error {
	table, tableExists, err := store.New(view).GetTableByID(ctx, reclamation.TableID)
	if err != nil {
		return err
	}
	switch reclamation.Kind {
	case model.ReclamationTable:
		if tableExists {
			return fmt.Errorf("catalog: reclamation %q targets live table %q", reclamation.ID, table.Name)
		}
	case model.ReclamationColumn:
		if tableExists {
			for _, column := range table.Columns {
				if column.ID == reclamation.ColumnID {
					return fmt.Errorf("catalog: reclamation %q targets live column %q.%q", reclamation.ID, table.Name, column.Name)
				}
			}
		}
	case model.ReclamationIndex:
		if tableExists {
			for _, index := range table.Indexes {
				if index.ID == reclamation.IndexID {
					return fmt.Errorf("catalog: reclamation %q targets live index %q on table %q", reclamation.ID, index.Name, table.Name)
				}
			}
		}
	case model.ReclamationTableDefinition:
		_, generation, ok, err := store.DefinitionHead(ctx, view, reclamation.TableSchemaID)
		if err != nil {
			return err
		}
		if ok && generation == reclamation.DefinitionGeneration {
			return fmt.Errorf("catalog: reclamation %q targets current table definition %d@%d",
				reclamation.ID, reclamation.TableSchemaID, generation)
		}
	case model.ReclamationWriteProtocolDefinition:
		generation, ok, err := store.WriteProtocolGeneration(ctx, view, reclamation.TableID)
		if err != nil {
			return err
		}
		if ok && generation == reclamation.WriteProtocolGeneration {
			return fmt.Errorf("catalog: reclamation %q targets current write protocol %s@%d",
				reclamation.ID, reclamation.TableID, generation)
		}
	case model.ReclamationTransitionDeltas, model.ReclamationCancelledIndex, model.ReclamationFailedIndex:
		transition, ok, err := store.GetTransition(ctx, view, reclamation.TransitionID)
		if err != nil {
			return err
		}
		if !ok {
			return fmt.Errorf("catalog: reclamation %q transition %q is missing", reclamation.ID, reclamation.TransitionID)
		}
		want := model.TransitionReady
		switch reclamation.Kind {
		case model.ReclamationCancelledIndex:
			want = model.TransitionCancelled
		case model.ReclamationFailedIndex:
			want = model.TransitionFailed
		}
		if transition.State != want || transition.TableID != reclamation.TableID || transition.Index.ID != reclamation.IndexID {
			return fmt.Errorf("catalog: reclamation %q target does not match terminal transition %q", reclamation.ID, reclamation.TransitionID)
		}
	case model.ReclamationReplacedColumn, model.ReclamationCancelledReplacement, model.ReclamationFailedReplacement:
		transition, ok, err := store.GetTransition(ctx, view, reclamation.TransitionID)
		if err != nil {
			return err
		}
		if !ok || transition.Kind != model.TransitionColumnReplacement || transition.ColumnReplacement == nil {
			return fmt.Errorf("catalog: reclamation %q replacement transition is missing", reclamation.ID)
		}
		wantState := model.TransitionReady
		wantColumn := transition.ColumnReplacement.Source.ID
		switch reclamation.Kind {
		case model.ReclamationCancelledReplacement:
			wantState = model.TransitionCancelled
			wantColumn = transition.ColumnReplacement.Target.ID
		case model.ReclamationFailedReplacement:
			wantState = model.TransitionFailed
			wantColumn = transition.ColumnReplacement.Target.ID
		}
		if transition.State != wantState || transition.TableID != reclamation.TableID ||
			reclamation.ColumnID != wantColumn {
			return fmt.Errorf("catalog: reclamation %q target does not match replacement transition %q", reclamation.ID, reclamation.TransitionID)
		}
		if tableExists {
			for _, column := range table.Columns {
				if column.ID == reclamation.ColumnID {
					return fmt.Errorf("catalog: reclamation %q targets active replacement column %q", reclamation.ID, column.Name)
				}
			}
		}
	case model.ReclamationConstraintValidation:
		transition, ok, err := store.GetTransition(ctx, view, reclamation.TransitionID)
		if err != nil {
			return err
		}
		if !ok || transition.Kind != model.TransitionConstraintValidation {
			return fmt.Errorf("catalog: reclamation %q constraint transition is missing", reclamation.ID)
		}
		switch transition.State {
		case model.TransitionReady, model.TransitionCancelled, model.TransitionFailed:
		default:
			return fmt.Errorf("catalog: reclamation %q constraint transition is not terminal", reclamation.ID)
		}
	default:
		return fmt.Errorf("catalog: reclamation %q has unknown kind %q", reclamation.ID, reclamation.Kind)
	}
	return nil
}

func applyReclamationBatch(ctx context.Context, txn kv.Txn, reclamation *model.Reclamation, batchSize int) (int, bool, error) {
	switch reclamation.Kind {
	case model.ReclamationTable:
		return reclaimTableBatch(ctx, txn, reclamation, batchSize)
	case model.ReclamationColumn:
		return reclaimColumnBatch(ctx, txn, reclamation, batchSize)
	case model.ReclamationIndex:
		return reclaimSingleRange(ctx, txn, reclamation, codec.IndexPrefix(reclamation.TableID, reclamation.IndexID), batchSize)
	case model.ReclamationTableDefinition:
		return reclaimExact(ctx, txn, store.TableDefinitionKey(reclamation.TableSchemaID, reclamation.DefinitionGeneration))
	case model.ReclamationWriteProtocolDefinition:
		return reclaimExact(ctx, txn, store.WriteProtocolDefinitionKey(reclamation.TableID, reclamation.WriteProtocolGeneration))
	case model.ReclamationTransitionDeltas:
		return reclaimTransitionBatch(ctx, txn, reclamation, batchSize, false)
	case model.ReclamationCancelledIndex, model.ReclamationFailedIndex:
		return reclaimTransitionBatch(ctx, txn, reclamation, batchSize, true)
	case model.ReclamationReplacedColumn, model.ReclamationCancelledReplacement, model.ReclamationFailedReplacement:
		return reclaimReplacementBatch(ctx, txn, reclamation, batchSize)
	case model.ReclamationConstraintValidation:
		start, end := store.TransitionViolationRange(reclamation.TransitionID)
		return reclaimRange(ctx, txn, start, end, reclamation, batchSize)
	default:
		return 0, false, fmt.Errorf("catalog: reclamation %q has unknown kind %q", reclamation.ID, reclamation.Kind)
	}
}

func reclaimReplacementBatch(
	ctx context.Context,
	txn kv.Txn,
	reclamation *model.Reclamation,
	batchSize int,
) (int, bool, error) {
	if reclamation.Phase == "" {
		reclamation.Phase = "column"
	}
	switch reclamation.Phase {
	case "column":
		count, done, err := reclaimColumnBatch(ctx, txn, reclamation, batchSize)
		if done {
			reclamation.Phase = "violations"
			reclamation.Cursor = nil
		}
		return count, false, err
	case "violations":
		start, end := store.TransitionViolationRange(reclamation.TransitionID)
		return reclaimRange(ctx, txn, start, end, reclamation, batchSize)
	default:
		return 0, false, fmt.Errorf(
			"catalog: replacement reclamation %q has invalid phase %q",
			reclamation.ID,
			reclamation.Phase,
		)
	}
}

func reclaimTableBatch(ctx context.Context, txn kv.Txn, reclamation *model.Reclamation, batchSize int) (int, bool, error) {
	if reclamation.Phase == "" {
		reclamation.Phase = "data"
	}
	switch {
	case reclamation.Phase == "data":
		count, done, err := reclaimRange(ctx, txn, codec.DataPrefix(reclamation.TableID), nil, reclamation, batchSize)
		if done {
			reclamation.Phase = "index:0"
			reclamation.Cursor = nil
		}
		return count, false, err
	case strings.HasPrefix(reclamation.Phase, "index:"):
		position, err := strconv.Atoi(strings.TrimPrefix(reclamation.Phase, "index:"))
		if err != nil || position < 0 {
			return 0, false, fmt.Errorf("catalog: reclamation %q has invalid phase %q", reclamation.ID, reclamation.Phase)
		}
		if position >= len(reclamation.IndexIDs) {
			reclamation.Phase = "definitions"
			reclamation.Cursor = nil
			return 0, false, nil
		}
		count, done, err := reclaimRange(ctx, txn, codec.IndexPrefix(reclamation.TableID, reclamation.IndexIDs[position]), nil, reclamation, batchSize)
		if done {
			reclamation.Phase = fmt.Sprintf("index:%d", position+1)
			reclamation.Cursor = nil
		}
		return count, false, err
	case reclamation.Phase == "definitions":
		start, end := store.TableDefinitionRange(reclamation.TableSchemaID)
		count, done, err := reclaimRange(ctx, txn, start, end, reclamation, batchSize)
		if done {
			reclamation.Phase = "write_protocols"
			reclamation.Cursor = nil
		}
		return count, false, err
	case reclamation.Phase == "write_protocols":
		start, end := store.WriteProtocolDefinitionRange(reclamation.TableID)
		count, done, err := reclaimRange(ctx, txn, start, end, reclamation, batchSize)
		return count, done, err
	default:
		return 0, false, fmt.Errorf("catalog: reclamation %q has invalid phase %q", reclamation.ID, reclamation.Phase)
	}
}

func reclaimColumnBatch(ctx context.Context, txn kv.Txn, reclamation *model.Reclamation, batchSize int) (int, bool, error) {
	prefix := codec.DataPrefix(reclamation.TableID)
	start := prefix
	if len(reclamation.Cursor) != 0 {
		start = append(bytes.Clone(reclamation.Cursor), 0)
	}
	it, err := txn.Scan(ctx, start, keyenc.PrefixEnd(prefix))
	if err != nil {
		return 0, false, err
	}
	defer it.Close()
	type rewrite struct{ key, value []byte }
	work := make([]rewrite, 0, batchSize)
	var cursor []byte
	for len(work) < batchSize && it.Next() {
		key := bytes.Clone(it.Key())
		value, changed, err := codec.RemoveColumn(it.Value(), reclamation.ColumnID)
		if err != nil {
			return 0, false, fmt.Errorf("catalog: reclaim column %s from row %q: %w", reclamation.ColumnID, key, err)
		}
		if changed {
			work = append(work, rewrite{key: key, value: value})
		} else {
			// Unchanged rows still advance the durable cursor and count toward
			// the bounded scan budget.
			work = append(work, rewrite{key: key})
		}
		cursor = key
	}
	if err := it.Err(); err != nil {
		return 0, false, err
	}
	for _, item := range work {
		if item.value != nil {
			if err := txn.Put(ctx, item.key, item.value); err != nil {
				return 0, false, err
			}
		}
	}
	if len(cursor) != 0 {
		reclamation.Cursor = cursor
	}
	return len(work), len(work) < batchSize, nil
}

func reclaimTransitionBatch(ctx context.Context, txn kv.Txn, reclamation *model.Reclamation, batchSize int, partialIndex bool) (int, bool, error) {
	if reclamation.Phase == "" {
		if partialIndex {
			reclamation.Phase = "index"
		} else {
			reclamation.Phase = "claims"
		}
	}
	if reclamation.Phase == "index" {
		count, done, err := reclaimRange(ctx, txn, codec.IndexPrefix(reclamation.TableID, reclamation.IndexID), nil, reclamation, batchSize)
		if done {
			reclamation.Phase = "claims"
			reclamation.Cursor = nil
		}
		return count, false, err
	}
	if reclamation.Phase == "claims" {
		start, end := store.UniqueClaimRange(reclamation.TransitionID)
		count, done, err := reclaimRange(ctx, txn, start, end, reclamation, batchSize)
		if done {
			reclamation.Phase = "violations"
			reclamation.Cursor = nil
		}
		return count, false, err
	}
	if reclamation.Phase == "violations" {
		start, end := store.UniqueViolationRange(reclamation.TransitionID)
		count, done, err := reclaimRange(ctx, txn, start, end, reclamation, batchSize)
		if done {
			reclamation.Phase = "deltas"
			reclamation.Cursor = nil
		}
		return count, false, err
	}
	if reclamation.Phase != "deltas" {
		return 0, false, fmt.Errorf("catalog: reclamation %q has invalid phase %q", reclamation.ID, reclamation.Phase)
	}
	start, end := store.DeltaRange(reclamation.TransitionID)
	count, done, err := reclaimRange(ctx, txn, start, end, reclamation, batchSize)
	if err != nil || !done {
		return count, false, err
	}
	if err := store.DeleteDeltaMetadata(ctx, txn, reclamation.TransitionID); err != nil {
		return count, false, err
	}
	return count + 2, true, nil
}

func reclaimSingleRange(ctx context.Context, txn kv.Txn, reclamation *model.Reclamation, prefix []byte, batchSize int) (int, bool, error) {
	return reclaimRange(ctx, txn, prefix, nil, reclamation, batchSize)
}

func reclaimRange(ctx context.Context, txn kv.Txn, start, end []byte, reclamation *model.Reclamation, batchSize int) (int, bool, error) {
	if end == nil {
		end = keyenc.PrefixEnd(start)
	}
	scanStart := start
	if len(reclamation.Cursor) != 0 {
		scanStart = append(bytes.Clone(reclamation.Cursor), 0)
	}
	it, err := txn.Scan(ctx, scanStart, end)
	if err != nil {
		return 0, false, err
	}
	defer it.Close()
	keys := make([][]byte, 0, batchSize)
	for len(keys) < batchSize && it.Next() {
		keys = append(keys, bytes.Clone(it.Key()))
	}
	if err := it.Err(); err != nil {
		return 0, false, err
	}
	for _, key := range keys {
		if err := txn.Delete(ctx, key); err != nil {
			return 0, false, err
		}
	}
	if len(keys) != 0 {
		reclamation.Cursor = keys[len(keys)-1]
	}
	return len(keys), len(keys) < batchSize, nil
}

func reclaimExact(ctx context.Context, txn kv.Txn, key []byte) (int, bool, error) {
	_, ok, err := txn.Get(ctx, key)
	if err != nil {
		return 0, false, err
	}
	if !ok {
		return 0, true, nil
	}
	if err := txn.Delete(ctx, key); err != nil {
		return 0, false, err
	}
	return 1, true, nil
}

func (e *Engine) failReclamation(ctx context.Context, id string, owner uint64, cause error) error {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	reclamation, ok, err := store.GetReclamation(ctx, txn, id)
	if err != nil || !ok {
		return err
	}
	if reclamation.OwnerEpoch != owner || reclamation.State == model.ReclamationReclaimed {
		return nil
	}
	reclamation.State = model.ReclamationFailed
	reclamation.Generation++
	reclamation.LastError = cause.Error()
	reclamation.UpdatedAt = time.Now().UTC()
	if err := store.SaveReclamation(ctx, txn, reclamation); err != nil {
		return err
	}
	return txn.Commit(ctx)
}
