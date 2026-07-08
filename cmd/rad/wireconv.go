package main

// Conversion between the wire protocol (plain JSON values, protocol types)
// and the engine's IR (typed lir values). All coercion is driven by the
// catalog's column types: JSON numbers become int64 or float64 according to
// the column, never by guessing.

import (
	"context"
	"encoding/json"
	"fmt"

	"rad/protocol"
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// wireErr is a client-caused conversion error (HTTP 400).
type wireErr struct{ msg string }

func (e wireErr) Error() string { return e.msg }

func wireErrf(format string, args ...any) error {
	return wireErr{fmt.Sprintf(format, args...)}
}

// coerceValue converts a JSON value into a typed lir.Value for col.
func coerceValue(col catalog.Column, v any) (lir.Value, error) {
	if v == nil {
		return lir.Null(col.Type), nil
	}
	switch col.Type {
	case catalog.TypeText:
		s, ok := v.(string)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a string, got %T", col.Name, v)
		}
		return lir.Text(s), nil
	case catalog.TypeInt64:
		n, ok := v.(json.Number)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a number, got %T", col.Name, v)
		}
		i, err := n.Int64()
		if err != nil {
			return lir.Value{}, wireErrf("column %q expects an integer, got %v", col.Name, n)
		}
		return lir.Int64(i), nil
	case catalog.TypeFloat64:
		n, ok := v.(json.Number)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a number, got %T", col.Name, v)
		}
		f, err := n.Float64()
		if err != nil {
			return lir.Value{}, wireErrf("column %q expects a float, got %v", col.Name, n)
		}
		return lir.Float64(f), nil
	case catalog.TypeBool:
		b, ok := v.(bool)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a boolean, got %T", col.Name, v)
		}
		return lir.Bool(b), nil
	}
	return lir.Value{}, wireErrf("column %q has unsupported type %q", col.Name, col.Type)
}

// coerceRow converts a JSON object into a typed row for tbl.
func coerceRow(tbl catalog.Table, values map[string]any) (lir.Row, error) {
	row := make(lir.Row, len(values))
	for name, v := range values {
		col, ok := tbl.Column(name)
		if !ok {
			return nil, wireErrf("table %q has no column %q", tbl.Name, name)
		}
		val, err := coerceValue(col, v)
		if err != nil {
			return nil, err
		}
		row[name] = val
	}
	return row, nil
}

// wireConv resolves tables once per request for include/filter coercion.
type wireConv struct {
	cat    *catalog.Catalog
	tables []catalog.Table // loaded lazily for include resolution
}

func (c *wireConv) table(ctx context.Context, name string) (catalog.Table, error) {
	tbl, ok, err := c.cat.GetTable(ctx, name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, wireErrf("table %q does not exist", name)
	}
	return tbl, nil
}

// toRead converts a wire Read into the IR, coercing every literal by the
// column types of the relation it filters.
func (c *wireConv) toRead(ctx context.Context, r protocol.Read) (lir.Read, error) {
	tbl, err := c.table(ctx, r.Table)
	if err != nil {
		return lir.Read{}, err
	}
	filter, err := c.toExpr(tbl, r.Filter)
	if err != nil {
		return lir.Read{}, err
	}
	includes, err := c.toIncludes(ctx, tbl, r.Include)
	if err != nil {
		return lir.Read{}, err
	}
	return lir.Read{
		Table:   r.Table,
		Filter:  filter,
		OrderBy: toOrder(r.OrderBy),
		Offset:  r.Offset,
		Limit:   r.Limit,
		Include: includes,
	}, nil
}

func toOrder(in []protocol.Order) []lir.OrderTerm {
	out := make([]lir.OrderTerm, len(in))
	for i, o := range in {
		out[i] = lir.OrderTerm{Column: o.Column, Desc: o.Desc}
	}
	return out
}

func (c *wireConv) toIncludes(ctx context.Context, tbl catalog.Table, in []protocol.Include) ([]lir.Include, error) {
	var out []lir.Include
	for _, inc := range in {
		child, dir, err := c.resolveInclude(ctx, tbl, inc)
		if err != nil {
			return nil, err
		}
		filter, err := c.toExpr(child, inc.Filter)
		if err != nil {
			return nil, err
		}
		nested, err := c.toIncludes(ctx, child, inc.Include)
		if err != nil {
			return nil, err
		}
		out = append(out, lir.Include{
			FK: inc.FK, Dir: dir, As: inc.As,
			Filter: filter, OrderBy: toOrder(inc.OrderBy), Limit: inc.Limit,
			Include: nested,
		})
	}
	return out, nil
}

// resolveInclude finds the table on the other side of an include so its
// filter literals can be coerced. (The planner re-resolves authoritatively.)
func (c *wireConv) resolveInclude(ctx context.Context, tbl catalog.Table, inc protocol.Include) (catalog.Table, lir.Direction, error) {
	switch inc.Dir {
	case string(lir.ToParent):
		for _, fk := range tbl.ForeignKeys {
			if fk.Name == inc.FK {
				parent, ok, err := c.cat.GetTableByID(ctx, fk.RefTableID)
				if err != nil {
					return catalog.Table{}, "", err
				}
				if !ok {
					return catalog.Table{}, "", wireErrf("foreign key %q references a missing table", inc.FK)
				}
				return parent, lir.ToParent, nil
			}
		}
		return catalog.Table{}, "", wireErrf("table %q has no foreign key %q", tbl.Name, inc.FK)

	case string(lir.ToChildren):
		if c.tables == nil {
			tables, err := c.cat.ListTables(ctx)
			if err != nil {
				return catalog.Table{}, "", err
			}
			c.tables = tables
		}
		for _, child := range c.tables {
			for _, fk := range child.ForeignKeys {
				if fk.Name == inc.FK && fk.RefTableID == tbl.ID {
					return child, lir.ToChildren, nil
				}
			}
		}
		return catalog.Table{}, "", wireErrf("no foreign key %q references table %q", inc.FK, tbl.Name)

	default:
		return catalog.Table{}, "", wireErrf("include %q: dir must be %q or %q", inc.As, lir.ToParent, lir.ToChildren)
	}
}

// toExpr converts a wire filter for one relation.
func (c *wireConv) toExpr(tbl catalog.Table, e *protocol.Expr) (lir.Expr, error) {
	if e == nil {
		return nil, nil
	}
	switch e.Op {
	case "and", "or":
		var subs []lir.Expr
		for i := range e.Exprs {
			sub, err := c.toExpr(tbl, &e.Exprs[i])
			if err != nil {
				return nil, err
			}
			subs = append(subs, sub)
		}
		if e.Op == "and" {
			return lir.And{Exprs: subs}, nil
		}
		return lir.Or{Exprs: subs}, nil

	case "not":
		if e.Expr == nil {
			return nil, wireErrf("not: missing expr")
		}
		sub, err := c.toExpr(tbl, e.Expr)
		if err != nil {
			return nil, err
		}
		return lir.Not{Expr: sub}, nil

	case "is_null":
		if _, ok := tbl.Column(e.Column); !ok {
			return nil, wireErrf("table %q has no column %q", tbl.Name, e.Column)
		}
		return lir.IsNull{Expr: lir.ColRef{Column: e.Column}}, nil

	case "eq", "ne", "lt", "lte", "gt", "gte":
		col, ok := tbl.Column(e.Column)
		if !ok {
			return nil, wireErrf("table %q has no column %q", tbl.Name, e.Column)
		}
		val, err := coerceValue(col, e.Value)
		if err != nil {
			return nil, err
		}
		left, right := lir.Expr(lir.ColRef{Column: e.Column}), lir.Expr(lir.Literal{Value: val})
		switch e.Op {
		case "eq":
			return lir.Eq{Left: left, Right: right}, nil
		case "ne":
			return lir.Cmp{Op: lir.OpNe, Left: left, Right: right}, nil
		case "lt":
			return lir.Cmp{Op: lir.OpLt, Left: left, Right: right}, nil
		case "lte":
			return lir.Cmp{Op: lir.OpLte, Left: left, Right: right}, nil
		case "gt":
			return lir.Cmp{Op: lir.OpGt, Left: left, Right: right}, nil
		case "gte":
			return lir.Cmp{Op: lir.OpGte, Left: left, Right: right}, nil
		}
	}
	return nil, wireErrf("unknown filter op %q", e.Op)
}
