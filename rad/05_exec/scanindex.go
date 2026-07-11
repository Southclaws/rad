package exec

// ScanIndex reads base rows through an index by equality prefix — the
// devtool's index-inspection primitive. Queries never call this; the planner
// chooses index access itself.

import (
	"context"
	"fmt"

	kv "rad/rad/01_kv"
	lir "rad/rad/03_lir"
)

// ScanIndex returns the base rows whose indexed values match prefix, which
// must populate a leading subset of the index's columns.
func (e *Engine) ScanIndex(ctx context.Context, table, index string, prefix lir.Row) ([]lir.Row, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, err
	}
	defer txn.Rollback()
	tbl, err := tableIn(ctx, txn, table)
	if err != nil {
		return nil, err
	}
	idx, ok := tbl.Index(index)
	if !ok {
		return nil, fmt.Errorf("exec: table %q has no index %q", table, index)
	}

	var eqVals []lir.Value
	for _, name := range idx.Columns {
		v, ok := prefix[name]
		if !ok {
			break
		}
		eqVals = append(eqVals, v)
	}
	if len(prefix) != len(eqVals) {
		return nil, fmt.Errorf("exec: index scan prefix must cover a leading subset of index %q columns %v", idx.Name, idx.Columns)
	}

	it, err := scanIndexRange(ctx, txn, tbl, idx, eqVals, nil)
	if err != nil {
		return nil, err
	}
	defer it.Close()

	var rows []lir.Row
	for {
		row, ok, err := it.Next()
		if err != nil {
			return nil, err
		}
		if !ok {
			return rows, nil
		}
		rows = append(rows, row)
	}
}
