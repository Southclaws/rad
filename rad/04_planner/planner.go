// Package planner lowers the logical IR (03_lir) into a physical plan.
//
// The physical plan is where storage-aware decisions live: which table and
// index a node reads (resolved against the catalog), which access path to
// use, and which join strategy to run. The executor (05_exec) walks the plan
// and performs KV operations; it never sees the IR's name strings.
//
// Today the lowering is a direct translation — every IR node has exactly one
// physical counterpart and joins are always nested-loop. This is where
// access-path selection, join reordering, predicate pushdown, and the rest
// of an optimizer will grow.
package planner

import (
	"context"
	"fmt"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// Node is one operator of a physical plan tree.
type Node interface {
	node()
}

// TableScan reads every row of a table in primary key order.
type TableScan struct {
	Table catalog.Table
	Alias string
}

// IndexScan reads base rows through a secondary index, optionally bounded by
// an equality prefix on the index's leading columns.
type IndexScan struct {
	Table catalog.Table
	Index catalog.Index
	Alias string
	// Prefix holds values for a leading subset of the index's columns.
	Prefix lir.Row
}

// Filter drops rows for which Expr is false.
type Filter struct {
	Input Node
	Expr  lir.Expr
}

// Project computes the output columns.
type Project struct {
	Input   Node
	Columns []lir.Projection
}

// NestedLoopJoin joins by materializing the right input and probing it once
// per left row.
//
// TODO: index nested-loop join — when the right side is an IndexScan whose
// leading index column is equated to a left column in On, re-scan the index
// per left row instead of materializing.
type NestedLoopJoin struct {
	Left  Node
	Right Node
	On    lir.Expr
}

// Limit stops after N rows.
type Limit struct {
	Input Node
	N     int
}

func (TableScan) node()      {}
func (IndexScan) node()      {}
func (Filter) node()         {}
func (Project) node()        {}
func (NestedLoopJoin) node() {}
func (Limit) node()          {}

// Plan lowers a logical query into a physical plan, resolving table and
// index names against the catalog.
func Plan(ctx context.Context, cat *catalog.Catalog, q lir.Query) (Node, error) {
	return lower(ctx, cat, q.Root)
}

func lower(ctx context.Context, cat *catalog.Catalog, node lir.RelNode) (Node, error) {
	switch n := node.(type) {
	case lir.Scan:
		if n.Alias == "" {
			return nil, fmt.Errorf("planner: scan of %q needs an alias", n.Table)
		}
		tbl, err := resolveTable(ctx, cat, n.Table)
		if err != nil {
			return nil, err
		}
		return TableScan{Table: tbl, Alias: n.Alias}, nil

	case lir.IndexScan:
		if n.Alias == "" {
			return nil, fmt.Errorf("planner: index scan of %q needs an alias", n.Table)
		}
		tbl, err := resolveTable(ctx, cat, n.Table)
		if err != nil {
			return nil, err
		}
		idx, ok := tbl.Index(n.Index)
		if !ok {
			return nil, fmt.Errorf("planner: table %q has no index %q", n.Table, n.Index)
		}
		if err := validatePrefix(idx, n.Prefix); err != nil {
			return nil, err
		}
		return IndexScan{Table: tbl, Index: idx, Alias: n.Alias, Prefix: n.Prefix}, nil

	case lir.Filter:
		in, err := lower(ctx, cat, n.Input)
		if err != nil {
			return nil, err
		}
		return Filter{Input: in, Expr: n.Expr}, nil

	case lir.Project:
		in, err := lower(ctx, cat, n.Input)
		if err != nil {
			return nil, err
		}
		for _, p := range n.Columns {
			if p.As == "" {
				return nil, fmt.Errorf("planner: projection needs an output name (As)")
			}
		}
		return Project{Input: in, Columns: n.Columns}, nil

	case lir.Join:
		if n.Type != lir.InnerJoin {
			return nil, fmt.Errorf("planner: unsupported join type %q", n.Type)
		}
		left, err := lower(ctx, cat, n.Left)
		if err != nil {
			return nil, err
		}
		right, err := lower(ctx, cat, n.Right)
		if err != nil {
			return nil, err
		}
		return NestedLoopJoin{Left: left, Right: right, On: n.On}, nil

	case lir.Limit:
		in, err := lower(ctx, cat, n.Input)
		if err != nil {
			return nil, err
		}
		return Limit{Input: in, N: n.N}, nil

	default:
		return nil, fmt.Errorf("planner: unsupported IR node %T", node)
	}
}

func resolveTable(ctx context.Context, cat *catalog.Catalog, name string) (catalog.Table, error) {
	tbl, ok, err := cat.GetTable(ctx, name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, fmt.Errorf("planner: table %q does not exist", name)
	}
	return tbl, nil
}

// validatePrefix checks that prefix covers a leading subset of the index's
// columns (all columns for an exact match, fewer for a range scan of the
// leading columns).
func validatePrefix(idx catalog.Index, prefix lir.Row) error {
	leading := 0
	for _, name := range idx.Columns {
		if _, ok := prefix[name]; !ok {
			break
		}
		leading++
	}
	if len(prefix) != leading {
		return fmt.Errorf("planner: index scan prefix must cover a leading subset of index %q columns %v", idx.Name, idx.Columns)
	}
	return nil
}
