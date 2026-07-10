package exec

// ScanIndex reads base rows through an index by equality prefix — the
// devtool's index-inspection primitive. Queries never call this; the planner
// chooses index access itself.

import (
	"context"
	"fmt"

	qir "rad/rad/03_qir"
)

// ScanIndex returns the base rows whose indexed values match prefix, which
// must populate a leading subset of the index's columns.
func (e *Engine) ScanIndex(ctx context.Context, table, index string, prefix qir.Row) ([]qir.Row, error) {
	tbl, err := e.table(ctx, table)
	if err != nil {
		return nil, err
	}
	idx, ok := tbl.Index(index)
	if !ok {
		return nil, fmt.Errorf("exec: table %q has no index %q", table, index)
	}

	var eqVals []qir.Value
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

	it, err := scanIndexRange(ctx, e.store, tbl, idx, eqVals, nil)
	if err != nil {
		return nil, err
	}
	defer it.Close()

	var rows []qir.Row
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
