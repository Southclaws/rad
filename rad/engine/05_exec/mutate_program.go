package exec

// Mutation statement execution for PIR programs. Each mutation consumes its
// input relation as one set and applies it with statement-boundary
// constraint checking: every row is written first, then the affected
// constraints are validated over the resulting state. That ordering is what
// makes the semantic upgrades in the design work — a create batch whose rows
// reference each other, and an update that swaps two unique values — because
// the checks see the whole post-statement state, not one row at a time.
//
// The input relation was fully evaluated before any write (the statement
// snapshot), so a mutation never observes its own effects while deriving its
// input.

import (
	"bytes"
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// applyCreate inserts every input row and returns the stored rows (defaults
// included). Primary-key collisions — within the batch or against existing
// rows — are rejected; uniqueness and foreign keys are checked once the whole
// batch is written, so rows may reference each other.
func (e *Engine) applyCreate(ctx context.Context, view kv.KV, tbl catalog.Table, inputRows []lir.Row) ([]lir.Row, error) {
	stored := make([]lir.Row, 0, len(inputRows))
	tuples := make([][]byte, 0, len(inputRows))
	seen := map[string]bool{}

	for _, in := range inputRows {
		withDefaults, err := applyDefaults(tbl, in)
		if err != nil {
			return nil, err
		}
		row, err := normalizeRow(tbl, withDefaults)
		if err != nil {
			return nil, err
		}
		pkTuple, err := encodeRowTuple(row, tbl.PrimaryKey)
		if err != nil {
			return nil, err
		}
		if seen[string(pkTuple)] {
			return nil, reject.Inputf("exec: create into %q has two rows with the same primary key", tbl.Name)
		}
		if _, ok, err := view.Get(ctx, DataKey(tbl.ID, pkTuple)); err != nil {
			return nil, err
		} else if ok {
			return nil, reject.Inputf("exec: duplicate primary key in table %q", tbl.Name)
		}
		seen[string(pkTuple)] = true
		if err := writeRowRaw(ctx, view, tbl, row, pkTuple); err != nil {
			return nil, err
		}
		stored = append(stored, row)
		tuples = append(tuples, pkTuple)
	}

	for i, row := range stored {
		if err := checkForeignKeys(ctx, view, tbl, row); err != nil {
			return nil, err
		}
		if err := checkUniqueIndexes(ctx, view, tbl, row, tuples[i]); err != nil {
			return nil, err
		}
	}
	return stored, nil
}

// applyUpdate identifies rows by the input relation's primary-key columns and
// assigns the remaining columns, returning the post-images. The whole batch
// is written before constraints are re-checked, so a statement may swap unique
// values between rows.
func (e *Engine) applyUpdate(ctx context.Context, view kv.KV, tbl catalog.Table, in lir.RowType, inputRows []lir.Row) ([]lir.Row, error) {
	assign, err := updateColumns(tbl, in)
	if err != nil {
		return nil, err
	}

	type pending struct {
		current, stored lir.Row
		pkTuple         []byte
		set             lir.Row
	}
	work := make([]pending, 0, len(inputRows))
	seen := map[string]bool{}

	for _, row := range inputRows {
		key := make(lir.Row, len(tbl.PrimaryKey))
		for _, pk := range tbl.PrimaryKey {
			key[pk] = row[pk]
		}
		current, pkTuple, err := loadByPK(ctx, view, tbl, key)
		if err != nil {
			return nil, err
		}
		if current == nil {
			return nil, reject.Fail(reject.ReasonMutationNotFound, "exec: update target not found in %q", tbl.Name)
		}
		if seen[string(pkTuple)] {
			return nil, reject.Fail(reject.ReasonMutationAmbiguous, "exec: update of %q identifies the same row twice", tbl.Name)
		}
		seen[string(pkTuple)] = true

		set := make(lir.Row, len(assign))
		merged := make(lir.Row, len(current))
		for k, v := range current {
			merged[k] = v
		}
		for _, col := range assign {
			merged[col] = row[col]
			set[col] = row[col]
		}
		stored, err := normalizeRow(tbl, merged)
		if err != nil {
			return nil, err
		}
		work = append(work, pending{current: current, stored: stored, pkTuple: pkTuple, set: set})
	}

	for _, w := range work {
		if err := writeRowUpdate(ctx, view, tbl, w.current, w.stored, w.pkTuple); err != nil {
			return nil, err
		}
	}
	for _, w := range work {
		if err := checkForeignKeysFor(ctx, view, tbl, w.stored, w.set); err != nil {
			return nil, err
		}
		if err := checkUniqueIndexesFor(ctx, view, tbl, w.stored, w.pkTuple, w.set); err != nil {
			return nil, err
		}
	}

	out := make([]lir.Row, len(work))
	for i, w := range work {
		out[i] = w.stored
	}
	return out, nil
}

// applyDelete removes rows identified by the input's primary-key columns and
// returns their pre-images. The referential check runs after the whole batch
// is deleted, so a program may delete a parent and its children together.
func (e *Engine) applyDelete(ctx context.Context, view kv.KV, tbl catalog.Table, in lir.RowType, inputRows []lir.Row) ([]lir.Row, error) {
	if err := deleteColumns(tbl, in); err != nil {
		return nil, err
	}
	type pending struct {
		current lir.Row
		pkTuple []byte
	}
	work := make([]pending, 0, len(inputRows))
	seen := map[string]bool{}

	for _, row := range inputRows {
		key := make(lir.Row, len(tbl.PrimaryKey))
		for _, pk := range tbl.PrimaryKey {
			key[pk] = row[pk]
		}
		current, pkTuple, err := loadByPK(ctx, view, tbl, key)
		if err != nil {
			return nil, err
		}
		if current == nil {
			return nil, reject.Fail(reject.ReasonMutationNotFound, "exec: delete target not found in %q", tbl.Name)
		}
		if seen[string(pkTuple)] {
			return nil, reject.Fail(reject.ReasonMutationAmbiguous, "exec: delete of %q identifies the same row twice", tbl.Name)
		}
		seen[string(pkTuple)] = true
		work = append(work, pending{current: current, pkTuple: pkTuple})
	}

	for _, w := range work {
		if err := deleteRowRaw(ctx, view, tbl, w.current, w.pkTuple); err != nil {
			return nil, err
		}
	}
	for _, w := range work {
		if err := e.checkNoReferences(ctx, view, tbl, w.current); err != nil {
			return nil, err
		}
	}

	out := make([]lir.Row, len(work))
	for i, w := range work {
		out[i] = w.current
	}
	return out, nil
}

// updateColumns validates an update input's schema and returns the assigned
// (non-primary-key) columns. Every primary-key column must be present to
// identify rows; at least one non-key column must be assigned; unknown or
// non-writable columns are rejected. Primary keys are immutable.
func updateColumns(tbl catalog.Table, in lir.RowType) ([]string, error) {
	present := map[string]bool{}
	for _, f := range in.Fields {
		if _, ok := tbl.Column(f.Name); !ok {
			return nil, reject.Inputf("exec: update of %q references unknown column %q", tbl.Name, f.Name)
		}
		if present[f.Name] {
			return nil, reject.Inputf("exec: update input has duplicate column %q", f.Name)
		}
		present[f.Name] = true
	}
	for _, pk := range tbl.PrimaryKey {
		if !present[pk] {
			return nil, reject.Inputf("exec: update of %q must include primary-key column %q to identify rows", tbl.Name, pk)
		}
	}
	var assign []string
	for _, f := range in.Fields {
		if isPrimaryKey(tbl, f.Name) {
			continue
		}
		assign = append(assign, f.Name)
	}
	if len(assign) == 0 {
		return nil, reject.Inputf("exec: update of %q assigns no columns — the input has only the primary key", tbl.Name)
	}
	return assign, nil
}

// deleteColumns validates that a delete input's schema is exactly the target's
// primary key — nothing more, nothing less.
func deleteColumns(tbl catalog.Table, in lir.RowType) error {
	if len(in.Fields) != len(tbl.PrimaryKey) {
		return reject.Inputf("exec: delete of %q needs exactly the primary-key columns", tbl.Name)
	}
	for _, f := range in.Fields {
		if !isPrimaryKey(tbl, f.Name) {
			return reject.Inputf("exec: delete of %q input column %q is not part of the primary key", tbl.Name, f.Name)
		}
	}
	return nil
}

func isPrimaryKey(tbl catalog.Table, col string) bool {
	for _, pk := range tbl.PrimaryKey {
		if pk == col {
			return true
		}
	}
	return false
}

// writeRowRaw writes a row's data and every index entry, without any
// constraint check — the batch runners validate afterwards.
func writeRowRaw(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte) error {
	rowBytes, err := MarshalRow(tbl, row)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range tbl.Indexes {
		idxTuple, err := encodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return nil
}

// writeRowUpdate overwrites a row's data and rewrites the index entries whose
// indexed values changed, without any constraint check.
func writeRowUpdate(ctx context.Context, view kv.KV, tbl catalog.Table, current, stored lir.Row, pkTuple []byte) error {
	rowBytes, err := MarshalRow(tbl, stored)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range tbl.Indexes {
		oldTuple, err := encodeRowTuple(current, idx.Columns)
		if err != nil {
			return err
		}
		newTuple, err := encodeRowTuple(stored, idx.Columns)
		if err != nil {
			return err
		}
		if bytes.Equal(oldTuple, newTuple) {
			continue
		}
		if err := view.Delete(ctx, IndexKey(tbl.ID, idx.ID, oldTuple, pkTuple)); err != nil {
			return err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, newTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return nil
}

// deleteRowRaw removes a row's data and every index entry, without the
// referential check — the batch runner validates afterwards.
func deleteRowRaw(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte) error {
	for _, idx := range tbl.Indexes {
		idxTuple, err := encodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		if err := view.Delete(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple)); err != nil {
			return err
		}
	}
	return view.Delete(ctx, DataKey(tbl.ID, pkTuple))
}
