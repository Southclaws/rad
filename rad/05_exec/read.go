package exec

// Shaped reads: execute a planner.ShapedRead into nested Records. This is
// the QIR path the generated client drives — access path fetch, residual
// filter, order, paginate, then per-row relationship fetches.

import (
	"context"
	"fmt"
	"slices"

	kv "rad/rad/01_kv"
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	planner "rad/rad/04_planner"
)

// Record is one result row with its included relations. Parents map include
// names to a single record (nil when the foreign key is NULL); Children map
// include names to arrays.
type Record struct {
	Columns  lir.Row
	Parents  map[string]*Record
	Children map[string][]*Record
}

// Read plans and executes a shaped read against committed state.
func (e *Engine) Read(ctx context.Context, r lir.Read) ([]*Record, error) {
	return e.read(ctx, e.store, r)
}

// Read inside a transaction sees its snapshot plus its own writes.
func (tx *Tx) Read(ctx context.Context, r lir.Read) ([]*Record, error) {
	return tx.e.read(ctx, tx.txn, r)
}

func (e *Engine) read(ctx context.Context, view kv.KV, r lir.Read) ([]*Record, error) {
	plan, err := planner.PlanRead(ctx, e.cat, r)
	if err != nil {
		return nil, err
	}
	return e.runShapedRead(ctx, view, plan)
}

func (e *Engine) runShapedRead(ctx context.Context, view kv.KV, plan *planner.ShapedRead) ([]*Record, error) {
	rows, err := fetchRows(ctx, view, plan.Table, plan.Access)
	if err != nil {
		return nil, err
	}
	rows, err = applyShaping(rows, plan.Filter, plan.OrderBy, plan.Offset, plan.Limit)
	if err != nil {
		return nil, err
	}

	records := make([]*Record, len(rows))
	for i, row := range rows {
		rec, err := e.attachIncludes(ctx, view, plan.Table, row, plan.Include)
		if err != nil {
			return nil, err
		}
		records[i] = rec
	}
	return records, nil
}

// fetchRows locates the base rows through the planned access path.
func fetchRows(ctx context.Context, view kv.KV, tbl catalog.Table, a planner.Access) ([]lir.Row, error) {
	switch a.Kind {
	case planner.AccessPKLookup:
		row, _, err := loadByPK(ctx, view, tbl, a.Key)
		if err != nil || row == nil {
			return nil, err
		}
		return []lir.Row{row}, nil
	case planner.AccessIndexScan:
		return scanIndex(ctx, view, tbl, a.Index, a.Prefix)
	case planner.AccessFullScan:
		it, err := scanTable(ctx, view, tbl)
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
	default:
		return nil, fmt.Errorf("exec: unknown access kind %q", a.Kind)
	}
}

// applyShaping filters, orders, and paginates rows.
func applyShaping(rows []lir.Row, filter lir.Expr, order []lir.OrderTerm, offset, limit int) ([]lir.Row, error) {
	if filter != nil {
		kept := rows[:0]
		for _, row := range rows {
			ok, err := evalRead(filter, row)
			if err != nil {
				return nil, err
			}
			if ok {
				kept = append(kept, row)
			}
		}
		rows = kept
	}

	if len(order) > 0 {
		var sortErr error
		slices.SortStableFunc(rows, func(a, b lir.Row) int {
			for _, t := range order {
				c, err := a[t.Column].Compare(b[t.Column])
				if err != nil && sortErr == nil {
					sortErr = err
				}
				if c != 0 {
					if t.Desc {
						return -c
					}
					return c
				}
			}
			return 0
		})
		if sortErr != nil {
			return nil, sortErr
		}
	}

	if offset > 0 {
		if offset >= len(rows) {
			return nil, nil
		}
		rows = rows[offset:]
	}
	if limit > 0 && len(rows) > limit {
		rows = rows[:limit]
	}
	return rows, nil
}

// attachIncludes builds the record for row and fetches its relations.
func (e *Engine) attachIncludes(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, includes []planner.ShapedInclude) (*Record, error) {
	rec := &Record{Columns: row}
	for _, inc := range includes {
		switch inc.Dir {
		case lir.ToParent:
			parent, err := e.fetchParent(ctx, view, row, inc)
			if err != nil {
				return nil, err
			}
			if rec.Parents == nil {
				rec.Parents = map[string]*Record{}
			}
			rec.Parents[inc.As] = parent
		case lir.ToChildren:
			kids, err := e.fetchChildren(ctx, view, tbl, row, inc)
			if err != nil {
				return nil, err
			}
			if rec.Children == nil {
				rec.Children = map[string][]*Record{}
			}
			rec.Children[inc.As] = kids
		}
	}
	return rec, nil
}

// fetchParent follows a foreign key on row to its referenced record; a NULL
// foreign key yields nil.
func (e *Engine) fetchParent(ctx context.Context, view kv.KV, row lir.Row, inc planner.ShapedInclude) (*Record, error) {
	key := make(lir.Row, len(inc.FK.RefColumns))
	for i, col := range inc.FK.Columns {
		v := row[col]
		if v.Null {
			return nil, nil
		}
		key[inc.FK.RefColumns[i]] = v
	}
	parentRow, _, err := loadByPK(ctx, view, inc.Child, key)
	if err != nil || parentRow == nil {
		return nil, err
	}
	return e.attachIncludes(ctx, view, inc.Child, parentRow, inc.Include)
}

// fetchChildren collects the rows of the referencing table whose foreign
// key points at row, then applies the include's shaping.
func (e *Engine) fetchChildren(ctx context.Context, view kv.KV, parent catalog.Table, row lir.Row, inc planner.ShapedInclude) ([]*Record, error) {
	want := make(lir.Row, len(inc.FK.Columns))
	for i, childCol := range inc.FK.Columns {
		want[childCol] = row[inc.FK.RefColumns[i]]
	}

	var rows []lir.Row
	var err error
	if inc.ChildIndex != nil {
		rows, err = scanIndex(ctx, view, inc.Child, *inc.ChildIndex, want)
	} else {
		rows, err = fetchRows(ctx, view, inc.Child, planner.Access{Kind: planner.AccessFullScan})
		if err == nil {
			kept := rows[:0]
			for _, r := range rows {
				match := true
				for col, v := range want {
					if !r[col].Equal(v) {
						match = false
						break
					}
				}
				if match {
					kept = append(kept, r)
				}
			}
			rows = kept
		}
	}
	if err != nil {
		return nil, err
	}

	rows, err = applyShaping(rows, inc.Filter, inc.OrderBy, 0, inc.Limit)
	if err != nil {
		return nil, err
	}

	kids := make([]*Record, len(rows))
	for i, childRow := range rows {
		rec, err := e.attachIncludes(ctx, view, inc.Child, childRow, inc.Include)
		if err != nil {
			return nil, err
		}
		kids[i] = rec
	}
	return kids, nil
}

// evalRead evaluates a read filter over a plain (unqualified) row.
// Comparisons involving NULL are false; NOT of false-because-NULL is true —
// acceptable POC three-valued-logic shortcut.
func evalRead(e lir.Expr, row lir.Row) (bool, error) {
	switch x := e.(type) {
	case lir.And:
		for _, sub := range x.Exprs {
			ok, err := evalRead(sub, row)
			if err != nil || !ok {
				return false, err
			}
		}
		return true, nil
	case lir.Or:
		for _, sub := range x.Exprs {
			ok, err := evalRead(sub, row)
			if err != nil {
				return false, err
			}
			if ok {
				return true, nil
			}
		}
		return false, nil
	case lir.Not:
		ok, err := evalRead(x.Expr, row)
		return !ok, err
	case lir.IsNull:
		v, err := readValue(x.Expr, row)
		if err != nil {
			return false, err
		}
		return v.Null, nil
	case lir.Eq:
		l, r, null, err := readOperands(x.Left, x.Right, row)
		if err != nil || null {
			return false, err
		}
		return l.Equal(r), nil
	case lir.Cmp:
		l, r, null, err := readOperands(x.Left, x.Right, row)
		if err != nil || null {
			return false, err
		}
		if x.Op == lir.OpNe {
			return !l.Equal(r), nil
		}
		c, err := l.Compare(r)
		if err != nil {
			return false, err
		}
		switch x.Op {
		case lir.OpLt:
			return c < 0, nil
		case lir.OpLte:
			return c <= 0, nil
		case lir.OpGt:
			return c > 0, nil
		case lir.OpGte:
			return c >= 0, nil
		}
		return false, fmt.Errorf("exec: unknown comparison %q", x.Op)
	default:
		return false, fmt.Errorf("exec: expression %T is not a read predicate", e)
	}
}

func readOperands(le, re lir.Expr, row lir.Row) (lir.Value, lir.Value, bool, error) {
	l, err := readValue(le, row)
	if err != nil {
		return lir.Value{}, lir.Value{}, false, err
	}
	r, err := readValue(re, row)
	if err != nil {
		return lir.Value{}, lir.Value{}, false, err
	}
	return l, r, l.Null || r.Null, nil
}

func readValue(e lir.Expr, row lir.Row) (lir.Value, error) {
	switch x := e.(type) {
	case lir.ColRef:
		v, ok := row[x.Column]
		if !ok {
			return lir.Value{}, fmt.Errorf("exec: unknown column %q", x.Column)
		}
		return v, nil
	case lir.Literal:
		return x.Value, nil
	default:
		return lir.Value{}, fmt.Errorf("exec: expression %T does not produce a value", e)
	}
}
