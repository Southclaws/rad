package mutate

// Mutation inputs are fully evaluated before any writes. Each batch is then
// written before its constraints are checked, so validation sees the complete
// post-statement state.

import (
	"context"
	"maps"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Constraints are checked after every row is visible, allowing rows in one
// statement to reference each other.
func Create(ctx context.Context, view kv.KV, tbl model.Table, inputRows []lir.Row) ([]lir.Row, error) {
	stored := make([]lir.Row, 0, len(inputRows))
	tuples := make([][]byte, 0, len(inputRows))
	seen := map[string]bool{}

	for _, in := range inputRows {
		row, err := prepare(tbl, in)
		if err != nil {
			return nil, err
		}
		pkTuple, err := codec.EncodeRowTuple(row, tbl.PrimaryKey)
		if err != nil {
			return nil, err
		}
		if seen[string(pkTuple)] {
			return nil, reject.Inputf("exec: create into %q has two rows with the same primary key", tbl.Name)
		}
		if _, ok, err := view.Get(ctx, codec.DataKey(tbl.ID, pkTuple)); err != nil {
			return nil, err
		} else if ok {
			return nil, reject.Inputf("exec: duplicate primary key in table %q", tbl.Name)
		}
		seen[string(pkTuple)] = true
		if err := rowstore.Write(ctx, view, tbl, row, pkTuple); err != nil {
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

// Rows are replaced before constraints are checked, allowing unique values to
// move between rows within one statement.
func Update(ctx context.Context, view kv.KV, tbl model.Table, in lir.RowType, inputRows []lir.Row) ([]lir.Row, error) {
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
		key := selectColumns(row, tbl.PrimaryKey)
		current, pkTuple, ok, err := rowstore.Get(ctx, view, tbl, key)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, reject.Fail(reject.ReasonMutationNotFound, "exec: update target not found in %q", tbl.Name)
		}
		if seen[string(pkTuple)] {
			return nil, reject.Fail(reject.ReasonMutationAmbiguous, "exec: update of %q identifies the same row twice", tbl.Name)
		}
		seen[string(pkTuple)] = true

		set := make(lir.Row, len(assign))
		merged := maps.Clone(current)
		for _, col := range assign {
			merged[col] = row[col]
			set[col] = row[col]
		}
		stored, err := normalize(tbl, merged)
		if err != nil {
			return nil, err
		}
		work = append(work, pending{current: current, stored: stored, pkTuple: pkTuple, set: set})
	}

	for _, w := range work {
		if err := rowstore.Replace(ctx, view, tbl, w.current, w.stored, w.pkTuple); err != nil {
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

// Referential checks run after every target row is removed, allowing related
// rows to be deleted by one statement.
func Delete(ctx context.Context, view kv.KV, tbl model.Table, in lir.RowType, inputRows []lir.Row) ([]lir.Row, error) {
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
		key := selectColumns(row, tbl.PrimaryKey)
		current, pkTuple, ok, err := rowstore.Get(ctx, view, tbl, key)
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, reject.Fail(reject.ReasonMutationNotFound, "exec: delete target not found in %q", tbl.Name)
		}
		if seen[string(pkTuple)] {
			return nil, reject.Fail(reject.ReasonMutationAmbiguous, "exec: delete of %q identifies the same row twice", tbl.Name)
		}
		seen[string(pkTuple)] = true
		work = append(work, pending{current: current, pkTuple: pkTuple})
	}

	for _, w := range work {
		if err := rowstore.Delete(ctx, view, tbl, w.current, w.pkTuple); err != nil {
			return nil, err
		}
	}
	for _, w := range work {
		if err := checkNoReferences(ctx, view, tbl, w.current); err != nil {
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
func updateColumns(tbl model.Table, in lir.RowType) ([]string, error) {
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
		if slices.Contains(tbl.PrimaryKey, f.Name) {
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
func deleteColumns(tbl model.Table, in lir.RowType) error {
	if len(in.Fields) != len(tbl.PrimaryKey) {
		return reject.Inputf("exec: delete of %q needs exactly the primary-key columns", tbl.Name)
	}
	for _, f := range in.Fields {
		if !slices.Contains(tbl.PrimaryKey, f.Name) {
			return reject.Inputf("exec: delete of %q input column %q is not part of the primary key", tbl.Name, f.Name)
		}
	}
	return nil
}

func selectColumns(row lir.Row, columns []string) lir.Row {
	selected := make(lir.Row, len(columns))
	for _, column := range columns {
		selected[column] = row[column]
	}
	return selected
}
