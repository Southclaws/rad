package planner

// Shaped-read planning: lowering lir.Read into a physical fetch plan. This
// is where access paths are chosen — the IR names no indexes; the planner
// extracts equality predicates from the filter and matches them against the
// primary key and secondary indexes.

import (
	"context"
	"fmt"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// ShapedRead is the physical plan for a lir.Read: how to fetch the root
// rows and how to attach each included relation.
type ShapedRead struct {
	Table  catalog.Table
	Access Access
	// Filter is the full residual predicate, evaluated on every fetched
	// row. The access path is an optimization; the filter remains the
	// source of truth.
	Filter  lir.Expr
	OrderBy []lir.OrderTerm
	Offset  int
	Limit   int
	Include []ShapedInclude
	// Aggs, when set, means the executor folds the fetched-and-filtered rows
	// into one scalar object rather than materialising records. The access
	// path above is chosen from Filter exactly as for a record read.
	Aggs []lir.AggTerm
}

// AccessKind is how the root rows are located.
type AccessKind string

const (
	// AccessFullScan walks the whole table in primary key order.
	AccessFullScan AccessKind = "full_scan"
	// AccessPKLookup fetches at most one row by complete primary key.
	AccessPKLookup AccessKind = "pk_lookup"
	// AccessIndexScan range-scans a secondary index by equality prefix.
	AccessIndexScan AccessKind = "index_scan"
)

type Access struct {
	Kind AccessKind
	// Key holds the complete primary key values (AccessPKLookup).
	Key lir.Row
	// Index and Prefix drive AccessIndexScan.
	Index  catalog.Index
	Prefix lir.Row
}

// ShapedInclude is a planned relationship fetch.
type ShapedInclude struct {
	As  string
	Dir lir.Direction
	// Child is the table on the other side of the relation: the referenced
	// table for ToParent, the referencing table for ToChildren.
	Child catalog.Table
	// FK is the resolved foreign key (defined on the referencing side).
	FK catalog.ForeignKey
	// ChildIndex, when non-nil, serves ToChildren fetches; otherwise the
	// executor falls back to scanning the child table.
	ChildIndex *catalog.Index

	Filter  lir.Expr
	OrderBy []lir.OrderTerm
	Limit   int
	Include []ShapedInclude
	// Aggs, when set, folds the matched children into one scalar object under
	// As instead of a record array.
	Aggs []lir.AggTerm
}

// PlanRead lowers a shaped read against the catalog.
func PlanRead(ctx context.Context, cat *catalog.Catalog, r lir.Read) (*ShapedRead, error) {
	tbl, err := resolveTable(ctx, cat, r.Table)
	if err != nil {
		return nil, err
	}
	if err := validateReadExprs(tbl, r.Filter, r.OrderBy); err != nil {
		return nil, err
	}
	if len(r.Aggs) > 0 {
		if len(r.OrderBy) > 0 || r.Offset != 0 || r.Limit != 0 || len(r.Include) > 0 {
			return nil, fmt.Errorf("planner: an aggregate query folds every matching row into one result, so it can't also order, paginate, or include relations — drop those, or run a separate read")
		}
		if err := validateAggs(tbl, r.Aggs); err != nil {
			return nil, err
		}
	}

	plan := &ShapedRead{
		Table:   tbl,
		Access:  chooseAccess(tbl, r.Filter),
		Filter:  r.Filter,
		OrderBy: r.OrderBy,
		Offset:  r.Offset,
		Limit:   r.Limit,
		Aggs:    r.Aggs,
	}
	plan.Include, err = planIncludes(ctx, cat, tbl, r.Include)
	if err != nil {
		return nil, err
	}
	return plan, nil
}

func planIncludes(ctx context.Context, cat *catalog.Catalog, tbl catalog.Table, includes []lir.Include) ([]ShapedInclude, error) {
	var out []ShapedInclude
	seen := map[string]bool{}
	for _, inc := range includes {
		if inc.As == "" {
			return nil, fmt.Errorf("planner: include of %q needs an output name (As)", inc.FK)
		}
		if seen[inc.As] {
			return nil, fmt.Errorf("planner: duplicate include name %q", inc.As)
		}
		seen[inc.As] = true

		si, err := planInclude(ctx, cat, tbl, inc)
		if err != nil {
			return nil, err
		}
		out = append(out, si)
	}
	return out, nil
}

func planInclude(ctx context.Context, cat *catalog.Catalog, tbl catalog.Table, inc lir.Include) (ShapedInclude, error) {
	si := ShapedInclude{As: inc.As, Dir: inc.Dir, Filter: inc.Filter, OrderBy: inc.OrderBy, Limit: inc.Limit}

	switch inc.Dir {
	case lir.ToParent:
		// The FK lives on tbl and points at the parent.
		fk, ok := findFK(tbl, inc.FK)
		if !ok {
			return ShapedInclude{}, fmt.Errorf("planner: table %q has no foreign key %q", tbl.Name, inc.FK)
		}
		parent, ok, err := cat.GetTableByID(ctx, fk.RefTableID)
		if err != nil {
			return ShapedInclude{}, err
		}
		if !ok {
			return ShapedInclude{}, fmt.Errorf("planner: foreign key %q references missing table %q", inc.FK, fk.RefTableID)
		}
		// A parent relation is at most one row, so filtering, ordering,
		// limiting, or folding it is never useful in V0 — a plain include
		// already gives you the single object (or null).
		if inc.Filter != nil || len(inc.OrderBy) > 0 || inc.Limit != 0 || len(inc.Aggs) > 0 {
			return ShapedInclude{}, fmt.Errorf("planner: include %q follows a parent relation, whose cardinality is zero-or-one, so filtering, ordering, limiting, or aggregating it does nothing — just include it", inc.As)
		}
		si.Child = parent
		si.FK = fk

	case lir.ToChildren:
		// The FK lives on some other table and references tbl.
		child, fk, err := findReferencingFK(ctx, cat, tbl, inc.FK)
		if err != nil {
			return ShapedInclude{}, err
		}
		si.Child = child
		si.FK = fk
		si.ChildIndex = indexCovering(child, fk.Columns)
		if err := validateReadExprs(child, inc.Filter, inc.OrderBy); err != nil {
			return ShapedInclude{}, err
		}
		if len(inc.Aggs) > 0 {
			if len(inc.OrderBy) > 0 || inc.Limit != 0 || len(inc.Include) > 0 {
				return ShapedInclude{}, fmt.Errorf("planner: include %q folds its matching rows into one result, so it can't also order, limit, or nest further includes — drop those, or use a separate record include", inc.As)
			}
			if err := validateAggs(child, inc.Aggs); err != nil {
				return ShapedInclude{}, err
			}
			si.Aggs = inc.Aggs
		}

	default:
		return ShapedInclude{}, fmt.Errorf("planner: include %q: unknown direction %q", inc.As, inc.Dir)
	}

	var err error
	si.Include, err = planIncludes(ctx, cat, si.Child, inc.Include)
	if err != nil {
		return ShapedInclude{}, err
	}
	return si, nil
}

func findFK(tbl catalog.Table, name string) (catalog.ForeignKey, bool) {
	for _, fk := range tbl.ForeignKeys {
		if fk.Name == name {
			return fk, true
		}
	}
	return catalog.ForeignKey{}, false
}

func findReferencingFK(ctx context.Context, cat *catalog.Catalog, tbl catalog.Table, fkName string) (catalog.Table, catalog.ForeignKey, error) {
	tables, err := cat.ListTables(ctx)
	if err != nil {
		return catalog.Table{}, catalog.ForeignKey{}, err
	}
	for _, child := range tables {
		for _, fk := range child.ForeignKeys {
			if fk.Name == fkName && fk.RefTableID == tbl.ID {
				return child, fk, nil
			}
		}
	}
	return catalog.Table{}, catalog.ForeignKey{}, fmt.Errorf("planner: no foreign key %q references table %q", fkName, tbl.Name)
}

// indexCovering returns an index whose leading columns equal cols, if any.
func indexCovering(tbl catalog.Table, cols []string) *catalog.Index {
	for i, idx := range tbl.Indexes {
		if len(idx.Columns) < len(cols) {
			continue
		}
		match := true
		for j, c := range cols {
			if idx.Columns[j] != c {
				match = false
				break
			}
		}
		if match {
			return &tbl.Indexes[i]
		}
	}
	return nil
}

// chooseAccess picks the cheapest access path the filter supports:
// a complete-PK equality set wins, then the index with the longest
// equality prefix, then a full scan.
func chooseAccess(tbl catalog.Table, filter lir.Expr) Access {
	eqs := equalities(filter)
	if len(eqs) == 0 {
		return Access{Kind: AccessFullScan}
	}

	if covers(eqs, tbl.PrimaryKey) {
		key := make(lir.Row, len(tbl.PrimaryKey))
		for _, c := range tbl.PrimaryKey {
			key[c] = eqs[c]
		}
		return Access{Kind: AccessPKLookup, Key: key}
	}

	best := Access{Kind: AccessFullScan}
	bestLen := 0
	for _, idx := range tbl.Indexes {
		n := 0
		for _, c := range idx.Columns {
			if _, ok := eqs[c]; !ok {
				break
			}
			n++
		}
		if n > bestLen {
			prefix := make(lir.Row, n)
			for _, c := range idx.Columns[:n] {
				prefix[c] = eqs[c]
			}
			best = Access{Kind: AccessIndexScan, Index: idx, Prefix: prefix}
			bestLen = n
		}
	}
	return best
}

// equalities extracts `column = literal` predicates from the top-level
// AND-tree of filter.
func equalities(e lir.Expr) map[string]lir.Value {
	out := map[string]lir.Value{}
	var walk func(lir.Expr)
	walk = func(e lir.Expr) {
		switch x := e.(type) {
		case lir.And:
			for _, sub := range x.Exprs {
				walk(sub)
			}
		case lir.Eq:
			if col, ok := x.Left.(lir.ColRef); ok && col.Alias == "" {
				if lit, ok := x.Right.(lir.Literal); ok && !lit.Value.Null {
					out[col.Column] = lit.Value
				}
			}
		}
	}
	if e != nil {
		walk(e)
	}
	return out
}

func covers(eqs map[string]lir.Value, cols []string) bool {
	for _, c := range cols {
		if _, ok := eqs[c]; !ok {
			return false
		}
	}
	return len(cols) > 0
}

// validateReadExprs checks that filter and order terms reference real
// columns of tbl.
func validateReadExprs(tbl catalog.Table, filter lir.Expr, order []lir.OrderTerm) error {
	for _, t := range order {
		if _, ok := tbl.Column(t.Column); !ok {
			return fmt.Errorf("planner: table %q has no column %q (order by)", tbl.Name, t.Column)
		}
	}
	var err error
	var walk func(lir.Expr)
	walk = func(e lir.Expr) {
		if err != nil {
			return
		}
		switch x := e.(type) {
		case lir.And:
			for _, sub := range x.Exprs {
				walk(sub)
			}
		case lir.Or:
			for _, sub := range x.Exprs {
				walk(sub)
			}
		case lir.Not:
			walk(x.Expr)
		case lir.IsNull:
			walk(x.Expr)
		case lir.Eq:
			walk(x.Left)
			walk(x.Right)
		case lir.Cmp:
			walk(x.Left)
			walk(x.Right)
		case lir.ColRef:
			if x.Alias != "" {
				err = fmt.Errorf("planner: read filters use bare column references, got %s.%s", x.Alias, x.Column)
				return
			}
			if _, ok := tbl.Column(x.Column); !ok {
				err = fmt.Errorf("planner: table %q has no column %q", tbl.Name, x.Column)
			}
		case lir.Literal, nil:
		default:
			err = fmt.Errorf("planner: unsupported expression %T in read filter", e)
		}
	}
	if filter != nil {
		walk(filter)
	}
	return err
}
