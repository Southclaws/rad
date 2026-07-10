package main

// compileRead lowers the wire protocol.Read into a relation-graph
// qir.Query: the table becomes a scan, includes become correlated
// sub-relations materialised by cardinality crossings in a projection, and
// aggregates become an Aggregate root. Literal values pass through raw —
// the binder owns all coercion.

import (
	"context"
	"fmt"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"

	"rad/protocol"
)

// readCompiler carries the catalog for foreign-key resolution and a counter
// for deterministic scope labels.
type readCompiler struct {
	cat    *catalog.Catalog
	tables []catalog.Table // lazy, for children-FK resolution
	n      int
}

func (c *readCompiler) scope() string {
	s := fmt.Sprintf("r%d", c.n)
	c.n++
	return s
}

// compileRead builds the query for one wire read.
func compileRead(ctx context.Context, cat *catalog.Catalog, r protocol.Read) (qir.Query, error) {
	c := &readCompiler{cat: cat}

	tbl, err := c.table(ctx, r.Table)
	if err != nil {
		return qir.Query{}, err
	}
	scope := c.scope()
	var rel qir.Relation = qir.Scan{Table: r.Table, Scope: scope}

	if r.Filter != nil {
		pred, err := compileExpr(scope, r.Filter)
		if err != nil {
			return qir.Query{}, err
		}
		rel = qir.Filter{Input: rel, Pred: pred}
	}

	// A root aggregate folds every matching row into one result — ordering,
	// pagination, and includes are meaningless against a single record.
	if len(r.Aggs) > 0 {
		if len(r.OrderBy) > 0 || r.Offset != 0 || r.Limit != 0 || len(r.Include) > 0 {
			return qir.Query{}, fmt.Errorf("planner: an aggregate query folds every matching row into one result, so it can't also order, paginate, or include relations — drop those, or run a separate read")
		}
		return qir.Query{
			Root: qir.Aggregate{Input: rel, Terms: compileAggs(scope, r.Aggs)},
			Card: qir.CardExactlyOne,
		}, nil
	}

	rel = compileShaping(scope, rel, r.OrderBy, r.Offset, r.Limit)

	if len(r.Include) > 0 {
		fields, err := c.compileIncludes(ctx, tbl, scope, r.Include)
		if err != nil {
			return qir.Query{}, err
		}
		rel = qir.Project{Input: rel, Spread: []string{scope}, Fields: fields}
	}
	return qir.Query{Root: rel, Card: qir.CardMany}, nil
}

// compileShaping appends order and slice nodes for the wire's flat fields;
// on this wire, limit 0 means unlimited.
func compileShaping(scope string, rel qir.Relation, orderBy []protocol.Order, offset, limit int) qir.Relation {
	if len(orderBy) > 0 {
		terms := make([]qir.OrderTerm, len(orderBy))
		for i, o := range orderBy {
			terms[i] = qir.OrderTerm{Expr: qir.Column{Scope: scope, Name: o.Column}, Desc: o.Desc}
		}
		rel = qir.Order{Input: rel, Terms: terms}
	}
	if offset != 0 || limit != 0 {
		var lim *int
		if limit != 0 {
			l := limit
			lim = &l
		}
		rel = qir.Slice{Input: rel, Offset: offset, Limit: lim}
	}
	return rel
}

// compileIncludes turns each include into a projection field: a correlated
// sub-relation crossed by cardinality — First for parents, Array for
// children, First-over-Aggregate for folded children.
func (c *readCompiler) compileIncludes(ctx context.Context, tbl catalog.Table, scope string, includes []protocol.Include) ([]qir.ProjField, error) {
	fields := make([]qir.ProjField, 0, len(includes))
	for _, inc := range includes {
		f, err := c.compileInclude(ctx, tbl, scope, inc)
		if err != nil {
			return nil, err
		}
		fields = append(fields, f)
	}
	return fields, nil
}

func (c *readCompiler) compileInclude(ctx context.Context, tbl catalog.Table, scope string, inc protocol.Include) (qir.ProjField, error) {
	switch inc.Dir {
	case "parent":
		if inc.Filter != nil || len(inc.OrderBy) > 0 || inc.Limit != 0 || len(inc.Aggs) > 0 {
			return qir.ProjField{}, fmt.Errorf("planner: include %q: a parent is at most one row, so it can't filter, order, limit, or aggregate", inc.As)
		}
		fk, ok := findFK(tbl, inc.FK)
		if !ok {
			return qir.ProjField{}, wireErrf("table %q has no foreign key %q", tbl.Name, inc.FK)
		}
		parent, ok, err := c.cat.GetTableByID(ctx, fk.RefTableID)
		if err != nil {
			return qir.ProjField{}, err
		}
		if !ok {
			return qir.ProjField{}, wireErrf("foreign key %q references a missing table", inc.FK)
		}
		ps := c.scope()
		var rel qir.Relation = qir.Filter{
			Input: qir.Scan{Table: parent.Name, Scope: ps},
			Pred:  joinPred(ps, fk.RefColumns, scope, fk.Columns),
		}
		if len(inc.Include) > 0 {
			nested, err := c.compileIncludes(ctx, parent, ps, inc.Include)
			if err != nil {
				return qir.ProjField{}, err
			}
			rel = qir.Project{Input: rel, Spread: []string{ps}, Fields: nested}
		}
		return qir.ProjField{As: inc.As, Expr: qir.First{Rel: rel}}, nil

	case "children":
		child, fk, err := c.findReferencingFK(ctx, tbl, inc.FK)
		if err != nil {
			return qir.ProjField{}, err
		}
		cs := c.scope()
		pred := joinPred(cs, fk.Columns, scope, fk.RefColumns)
		if inc.Filter != nil {
			user, err := compileExpr(cs, inc.Filter)
			if err != nil {
				return qir.ProjField{}, err
			}
			pred = qir.Binary{Op: qir.OpAnd, L: pred, R: user}
		}
		var rel qir.Relation = qir.Filter{
			Input: qir.Scan{Table: child.Name, Scope: cs},
			Pred:  pred,
		}

		// A folded children include renders as one object of scalars —
		// exactly First over a global fold.
		if len(inc.Aggs) > 0 {
			if len(inc.OrderBy) > 0 || inc.Limit != 0 || len(inc.Include) > 0 {
				return qir.ProjField{}, fmt.Errorf("planner: include %q: a fold covers the whole matching set, so it can't also order, limit, or include", inc.As)
			}
			return qir.ProjField{As: inc.As, Expr: qir.First{
				Rel: qir.Aggregate{Input: rel, Terms: compileAggs(cs, inc.Aggs)},
			}}, nil
		}

		rel = compileShaping(cs, rel, inc.OrderBy, 0, inc.Limit)
		if len(inc.Include) > 0 {
			nested, err := c.compileIncludes(ctx, child, cs, inc.Include)
			if err != nil {
				return qir.ProjField{}, err
			}
			rel = qir.Project{Input: rel, Spread: []string{cs}, Fields: nested}
		}
		return qir.ProjField{As: inc.As, Expr: qir.Array{Rel: rel}}, nil

	default:
		return qir.ProjField{}, wireErrf("include %q: dir must be %q or %q", inc.As, "parent", "children")
	}
}

// joinPred equates the two sides of a foreign key, column by column.
func joinPred(ls string, lcols []string, rs string, rcols []string) qir.Expr {
	var pred qir.Expr
	for i := range lcols {
		eq := qir.Binary{Op: qir.OpEq,
			L: qir.Column{Scope: ls, Name: lcols[i]},
			R: qir.Column{Scope: rs, Name: rcols[i]},
		}
		if pred == nil {
			pred = eq
		} else {
			pred = qir.Binary{Op: qir.OpAnd, L: pred, R: eq}
		}
	}
	return pred
}

func compileAggs(scope string, aggs []protocol.Agg) []qir.AggTerm {
	out := make([]qir.AggTerm, len(aggs))
	for i, a := range aggs {
		t := qir.AggTerm{Fn: qir.AggFn(a.Fn), As: a.As}
		if a.Column != "" {
			t.Arg = qir.Column{Scope: scope, Name: a.Column}
		}
		out[i] = t
	}
	return out
}

// compileExpr lowers the wire's flat tagged-union filter. Values stay raw;
// the binder coerces them against the columns they meet.
func compileExpr(scope string, e *protocol.Expr) (qir.Expr, error) {
	switch e.Op {
	case "and", "or":
		if len(e.Exprs) == 0 {
			return nil, wireErrf("%s needs operands", e.Op)
		}
		op := qir.OpAnd
		if e.Op == "or" {
			op = qir.OpOr
		}
		out, err := compileExpr(scope, &e.Exprs[0])
		if err != nil {
			return nil, err
		}
		for i := 1; i < len(e.Exprs); i++ {
			r, err := compileExpr(scope, &e.Exprs[i])
			if err != nil {
				return nil, err
			}
			out = qir.Binary{Op: op, L: out, R: r}
		}
		return out, nil

	case "not":
		if e.Expr == nil {
			return nil, wireErrf("not needs an expression")
		}
		sub, err := compileExpr(scope, e.Expr)
		if err != nil {
			return nil, err
		}
		return qir.Unary{Op: qir.OpNot, X: sub}, nil

	case "is_null":
		return qir.Unary{Op: qir.OpIsNull, X: qir.Column{Scope: scope, Name: e.Column}}, nil

	case "eq", "ne", "lt", "lte", "gt", "gte":
		return qir.Binary{
			Op: qir.BinaryOp(e.Op),
			L:  qir.Column{Scope: scope, Name: e.Column},
			R:  qir.Literal{Raw: e.Value},
		}, nil

	default:
		return nil, wireErrf("unknown filter op %q", e.Op)
	}
}

func findFK(tbl catalog.Table, name string) (catalog.ForeignKey, bool) {
	for _, fk := range tbl.ForeignKeys {
		if fk.Name == name {
			return fk, true
		}
	}
	return catalog.ForeignKey{}, false
}

func (c *readCompiler) findReferencingFK(ctx context.Context, tbl catalog.Table, name string) (catalog.Table, catalog.ForeignKey, error) {
	if c.tables == nil {
		tables, err := c.cat.ListTables(ctx)
		if err != nil {
			return catalog.Table{}, catalog.ForeignKey{}, err
		}
		c.tables = tables
	}
	for _, child := range c.tables {
		for _, fk := range child.ForeignKeys {
			if fk.Name == name && fk.RefTableID == tbl.ID {
				return child, fk, nil
			}
		}
	}
	return catalog.Table{}, catalog.ForeignKey{}, wireErrf("no foreign key %q references table %q", name, tbl.Name)
}

func (c *readCompiler) table(ctx context.Context, name string) (catalog.Table, error) {
	tbl, ok, err := c.cat.GetTable(ctx, name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, wireErrf("table %q does not exist", name)
	}
	return tbl, nil
}
