package api

// Raise a nested engine query back into the flat wire form — the inverse of
// lowerQuery. Every relation becomes one named node; nested inputs and crossing
// sub-relations become their own nodes, referenced by name. The server never
// needs this (requests arrive as wire and are lowered), but tooling that holds
// an engine query and must serialise it as wire — the generative suite emitting
// a failing case as a fixture — does.

import (
	"fmt"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// WireQuery lowers a nested engine query into the wire node-map form.
func WireQuery(q lir.Query) lirwire.Query {
	w := &raiser{nodes: map[string]lirwire.Node{}}
	root := w.rel(q.Root)
	var bindings map[string]lirwire.Binding
	if len(q.Bindings) > 0 {
		bindings = make(map[string]lirwire.Binding, len(q.Bindings))
		for name, b := range q.Bindings {
			if rec, ok := b.(lir.Recursive); ok {
				bindings[name] = lirwire.Recursive(w.rel(rec.Anchor), w.rel(rec.Step), string(rec.Accumulation))
				continue
			}
			bindings[name] = lirwire.Derived(w.rel(b))
		}
	}
	return lirwire.Query{
		Nodes:    w.nodes,
		Root:     lirwire.Root{Node: root, Cardinality: string(q.Card)},
		Bindings: bindings,
	}
}

type raiser struct {
	nodes map[string]lirwire.Node
	n     int
}

func (w *raiser) name() string { w.n++; return fmt.Sprintf("n%d", w.n) }

// rel lowers one relation, registering it (and, by recursion, its inputs and
// crossing sub-relations) in the node map, and returns its assigned name.
func (w *raiser) rel(r lir.Relation) string {
	var node lirwire.Node
	switch x := r.(type) {
	case lir.Scan:
		node = lirwire.Scan(x.Table, x.Scope)
	case lir.Rows:
		cols := make([]lirwire.RowsColumn, len(x.Columns))
		for i, c := range x.Columns {
			cols[i] = lirwire.RowsColumn{Name: c.Name, Type: lirwire.ScalarType(string(c.Kind)), Nullable: wireBoolPtr(c.Nullable)}
		}
		rows := make([][]lirwire.Cell, len(x.Values))
		for i, row := range x.Values {
			cells := make([]lirwire.Cell, len(row))
			for j, v := range row {
				cell, err := lirwire.MakeCell(cols[j].Type, v)
				if err != nil {
					panic(fmt.Sprintf("api: rows cell [%d][%d]: %v", i, j, err))
				}
				cells[j] = cell
			}
			rows[i] = cells
		}
		node = lirwire.Rows(x.Scope, cols, rows)
	case lir.Filter:
		node = lirwire.Filter(w.rel(x.Input), w.expr(x.Pred))
	case lir.Project:
		fields := make([]lirwire.Field, len(x.Fields))
		for i, f := range x.Fields {
			fields[i] = lirwire.Field{As: f.As, Expr: w.expr(f.Expr)}
		}
		node = lirwire.Project(w.rel(x.Input), x.Scope, x.Spread, fields)
	case lir.Join:
		node = lirwire.Join(w.rel(x.Left), w.rel(x.Right), string(x.Kind), w.expr(x.On))
	case lir.Concatenate:
		inputs := make([]string, len(x.Inputs))
		for i, in := range x.Inputs {
			inputs[i] = w.rel(in)
		}
		node = lirwire.Concatenate(x.Scope, inputs...)
	case lir.Intersect:
		node = lirwire.Intersect(x.Scope, w.rel(x.Left), w.rel(x.Right), string(x.Quantifier))
	case lir.Except:
		node = lirwire.Except(x.Scope, w.rel(x.Left), w.rel(x.Right), string(x.Quantifier))
	case lir.Aggregate:
		groups := make([]lirwire.GroupTerm, len(x.Groups))
		for i, g := range x.Groups {
			groups[i] = lirwire.GroupTerm{As: wireStrPtr(g.As), Expr: w.expr(g.Expr)}
		}
		aggs := make([]lirwire.AggTerm, len(x.Terms))
		for i, a := range x.Terms {
			at := lirwire.AggTerm{Fn: string(a.Fn), As: a.As}
			if a.Arg != nil {
				e := w.expr(a.Arg)
				at.Arg = &e
			}
			aggs[i] = at
		}
		node = lirwire.Aggregate(w.rel(x.Input), x.Scope, groups, aggs)
	case lir.Order:
		terms := make([]lirwire.OrderTerm, len(x.Terms))
		for i, ot := range x.Terms {
			term := lirwire.OrderTerm{Expr: w.expr(ot.Expr)}
			if ot.Desc {
				desc := true
				term.Desc = &desc
			}
			terms[i] = term
		}
		node = lirwire.Order(w.rel(x.Input), terms)
	case lir.Slice:
		node = lirwire.Slice(w.rel(x.Input), x.Offset, x.Limit)
	case lir.Ref:
		node = lirwire.Ref(x.Binding, x.Scope)
	case lir.RecursiveRef:
		node = lirwire.RecursiveRef(x.Binding, x.Scope)
	case lir.Distinct:
		node = lirwire.Distinct(w.rel(x.Input))
	default:
		panic(fmt.Sprintf("api: unknown relation %T", r))
	}
	name := w.name()
	w.nodes[name] = node
	return name
}

func (w *raiser) expr(e lir.Expr) lirwire.Expr {
	switch x := e.(type) {
	case lir.Column:
		return lirwire.Col(x.Scope, x.Name)
	case lir.Literal:
		if x.Raw == nil && x.Kind != "" {
			return lirwire.Lit(lirwire.Null(lirwire.ScalarType(string(x.Kind))))
		}
		return lirwire.LitOf(x.Raw)
	case lir.Unary:
		return lirwire.Unary(string(x.Op), w.expr(x.X))
	case lir.Binary:
		return lirwire.Binary(string(x.Op), w.expr(x.L), w.expr(x.R))
	case lir.Cast:
		return lirwire.Cast(w.expr(x.X), lirwire.ScalarType(string(x.To)))
	case lir.Exists:
		return lirwire.Exists(w.rel(x.Rel))
	case lir.First:
		return lirwire.First(w.rel(x.Rel))
	case lir.Scalar:
		return lirwire.Scalar(w.rel(x.Rel))
	case lir.Array:
		return lirwire.Array(w.rel(x.Rel))
	default:
		panic(fmt.Sprintf("api: unknown expression %T", e))
	}
}

// wireBoolPtr and wireStrPtr render optional wire fields present only when
// meaningful, matching how the client builds them (an absent flag reads as
// false/unset).
func wireBoolPtr(b bool) *bool {
	if !b {
		return nil
	}
	return &b
}

func wireStrPtr(s string) *string {
	if s == "" {
		return nil
	}
	return &s
}
