package api

// Lower a wire LIR query — the Schemancer-generated union, with string node
// references — into the engine's unbound relation IR. This is mechanical:
// dispatch on the union variant, resolve references, decode raw literal bytes.
//
// It validates nothing beyond the structural minimum the lowering itself needs
// (a present predicate/condition, a resolvable reference, no reference cycle).
// The LIR schema has already rejected structurally malformed documents on the
// wire, and names, types, cardinality, the single-consumer forest law, binding
// cycles, and reachability are all the engine binder's job.

import (
	"slices"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// lowerQuery materialises a wire query into an unbound lir.Query. A statement's
// `ref` nodes may resolve to earlier statement results as well as its own
// bindings; the lowering treats every ref uniformly and leaves resolution to
// the binder.
func lowerQuery(q lirwire.Query) (lir.Query, error) {
	g := &graphConv{nodes: q.Nodes, building: map[string]bool{}, reached: map[string]bool{}}
	root, err := g.rel(q.Root.Node)
	if err != nil {
		return lir.Query{}, err
	}
	var bindings map[string]lir.Relation
	if len(q.Bindings) > 0 {
		bindings = make(map[string]lir.Relation, len(q.Bindings))
		for name, b := range q.Bindings {
			body, err := g.binding(b)
			if err != nil {
				return lir.Query{}, err
			}
			bindings[name] = body
		}
	}
	// Reachability is a wire-graph property the binder can no longer see (it
	// receives only the lowered tree). An orphan node — reachable from neither
	// the root nor any binding root — is a dead definition and always a
	// mistake, so reject it here where the whole node map is still in hand.
	if len(g.reached) != len(q.Nodes) {
		var orphans []string
		for name := range q.Nodes {
			if !g.reached[name] {
				orphans = append(orphans, name)
			}
		}
		slices.Sort(orphans)
		return lir.Query{}, wireErrf("unreachable node definitions: %v", orphans)
	}
	return lir.Query{Root: root, Card: lir.RootCard(q.Root.Cardinality), Bindings: bindings}, nil
}

type graphConv struct {
	nodes    map[string]lirwire.Node
	building map[string]bool // in-progress set: a re-entry is a cycle
	reached  map[string]bool // every node visited from a root — the rest are orphans
}

// rel resolves one node reference into a relation value. Value nodes cannot
// hold cycles, so a reference re-entered while still being built is a cycle
// in the wire graph.
func (g *graphConv) rel(name string) (lir.Relation, error) {
	if name == "" {
		return nil, wireErrf("missing node reference")
	}
	n, ok := g.nodes[name]
	if !ok {
		return nil, wireErrf("unknown node %q", name)
	}
	g.reached[name] = true
	if g.building[name] {
		return nil, wireErrf("node %q is part of a cycle", name)
	}
	g.building[name] = true
	defer delete(g.building, name)

	switch x := n.NodeUnion.(type) {
	case *lirwire.ScanNode:
		return lir.Scan{Table: x.Table, Scope: x.Scope}, nil

	case *lirwire.RowsNode:
		cols := make([]lir.RowsCol, len(x.Columns))
		for i, c := range x.Columns {
			cols[i] = lir.RowsCol{Name: c.Name, Kind: lir.Kind(string(c.Type)), Nullable: optBool(c.Nullable)}
		}
		values := make([][]any, len(x.Rows))
		for i, row := range x.Rows {
			if len(row) != len(x.Columns) {
				return nil, wireErrf("node %q row %d has %d cells, want %d", name, i, len(row), len(x.Columns))
			}
			cells := make([]any, len(row))
			for j, cell := range row {
				v, err := decodeCell(x.Columns[j].Type, cell)
				if err != nil {
					return nil, wireErrf("node %q rows cell [%d][%d]: %v", name, i, j, err)
				}
				cells[j] = v
			}
			values[i] = cells
		}
		return lir.Rows{Scope: x.Scope, Columns: cols, Values: values}, nil

	case *lirwire.FilterNode:
		in, err := g.rel(x.Input)
		if err != nil {
			return nil, err
		}
		pred, err := g.expr(x.Predicate)
		if err != nil {
			return nil, err
		}
		if pred == nil {
			return nil, wireErrf("filter %q needs a predicate", name)
		}
		return lir.Filter{Input: in, Pred: pred}, nil

	case *lirwire.ProjectNode:
		in, err := g.rel(x.Input)
		if err != nil {
			return nil, err
		}
		fields := make([]lir.ProjField, len(x.Fields))
		for i, f := range x.Fields {
			e, err := g.expr(f.Expr)
			if err != nil {
				return nil, err
			}
			fields[i] = lir.ProjField{As: f.As, Expr: e}
		}
		return lir.Project{Input: in, Scope: optString(x.Scope), Spread: x.Spread, Fields: fields}, nil

	case *lirwire.JoinNode:
		l, err := g.rel(x.Left)
		if err != nil {
			return nil, err
		}
		r, err := g.rel(x.Right)
		if err != nil {
			return nil, err
		}
		on, err := g.expr(x.On)
		if err != nil {
			return nil, err
		}
		if on == nil {
			return nil, wireErrf("join %q needs a condition", name)
		}
		return lir.Join{Left: l, Right: r, Kind: lir.JoinKind(x.Join), On: on}, nil

	case *lirwire.ConcatenateNode:
		inputs := make([]lir.Relation, len(x.Inputs))
		for i, ref := range x.Inputs {
			in, err := g.rel(ref)
			if err != nil {
				return nil, err
			}
			inputs[i] = in
		}
		return lir.Concatenate{Scope: x.Scope, Inputs: inputs}, nil

	case *lirwire.IntersectNode:
		l, err := g.rel(x.Left)
		if err != nil {
			return nil, err
		}
		r, err := g.rel(x.Right)
		if err != nil {
			return nil, err
		}
		return lir.Intersect{Scope: x.Scope, Left: l, Right: r, Quantifier: lir.SetQuantifier(x.Quantifier)}, nil

	case *lirwire.ExceptNode:
		l, err := g.rel(x.Left)
		if err != nil {
			return nil, err
		}
		r, err := g.rel(x.Right)
		if err != nil {
			return nil, err
		}
		return lir.Except{Scope: x.Scope, Left: l, Right: r, Quantifier: lir.SetQuantifier(x.Quantifier)}, nil

	case *lirwire.AggregateNode:
		in, err := g.rel(x.Input)
		if err != nil {
			return nil, err
		}
		groups := make([]lir.GroupTerm, len(x.Groups))
		for i, gt := range x.Groups {
			e, err := g.expr(gt.Expr)
			if err != nil {
				return nil, err
			}
			groups[i] = lir.GroupTerm{As: optString(gt.As), Expr: e}
		}
		terms := make([]lir.AggTerm, len(x.Aggs))
		for i, a := range x.Aggs {
			var arg lir.Expr
			if a.Arg != nil {
				arg, err = g.expr(*a.Arg)
				if err != nil {
					return nil, err
				}
			}
			terms[i] = lir.AggTerm{Fn: lir.AggFn(a.Fn), Arg: arg, As: a.As}
		}
		return lir.Aggregate{Input: in, Scope: optString(x.Scope), Groups: groups, Terms: terms}, nil

	case *lirwire.OrderNode:
		in, err := g.rel(x.Input)
		if err != nil {
			return nil, err
		}
		terms := make([]lir.OrderTerm, len(x.Terms))
		for i, t := range x.Terms {
			e, err := g.expr(t.Expr)
			if err != nil {
				return nil, err
			}
			terms[i] = lir.OrderTerm{Expr: e, Desc: optBool(t.Desc)}
		}
		return lir.Order{Input: in, Terms: terms}, nil

	case *lirwire.SliceNode:
		in, err := g.rel(x.Input)
		if err != nil {
			return nil, err
		}
		offset := 0
		if x.Offset != nil {
			offset = *x.Offset
		}
		return lir.Slice{Input: in, Offset: offset, Limit: x.Limit}, nil

	case *lirwire.RefNode:
		return lir.Ref{Binding: x.Binding, Scope: x.Scope}, nil

	case *lirwire.RecursiveRefNode:
		return lir.RecursiveRef{Binding: x.Binding, Scope: x.Scope}, nil

	case *lirwire.DistinctNode:
		in, err := g.rel(x.Input)
		if err != nil {
			return nil, err
		}
		return lir.Distinct{Input: in}, nil

	default:
		return nil, wireErrf("node %q has unknown kind %q", name, nodeKind(n))
	}
}

// binding lowers a wire binding: a derived binding is its defining tree; a
// recursive binding lowers to a lir.Recursive body carrying its anchor and
// step. Both branches reach their nodes through g.rel, so orphan detection
// still sees them.
func (g *graphConv) binding(b lirwire.Binding) (lir.Relation, error) {
	switch x := b.BindingUnion.(type) {
	case *lirwire.DerivedBinding:
		return g.rel(x.Node)
	case *lirwire.RecursiveBinding:
		anchor, err := g.rel(x.Anchor)
		if err != nil {
			return nil, err
		}
		step, err := g.rel(x.Step)
		if err != nil {
			return nil, err
		}
		return lir.Recursive{Anchor: anchor, Step: step, Accumulation: lir.RecursiveAccumulationMode(x.Accumulation)}, nil
	default:
		return nil, wireErrf("binding has unknown kind")
	}
}

func (g *graphConv) expr(e lirwire.Expr) (lir.Expr, error) {
	switch x := e.ExprUnion.(type) {
	case nil:
		return nil, nil

	case *lirwire.LiteralExpr:
		raw, err := decodeValue(x.Value)
		if err != nil {
			return nil, wireErrf("literal: %v", err)
		}
		lit := lir.Literal{Raw: raw}
		if raw == nil {
			// A wire NULL is always typed; carry that type so a projected NULL,
			// which meets no column context, still binds.
			lit.Kind = lir.Kind(string(valueScalarType(x.Value)))
		}
		return lit, nil

	case *lirwire.ColumnExpr:
		return lir.Column{Scope: x.Scope, Name: x.Column}, nil

	case *lirwire.UnaryExpr:
		sub, err := g.expr(x.Expr)
		if err != nil {
			return nil, err
		}
		if sub == nil {
			return nil, wireErrf("unary %q needs an operand", x.Op)
		}
		return lir.Unary{Op: lir.UnaryOp(x.Op), X: sub}, nil

	case *lirwire.BinaryExpr:
		l, err := g.expr(x.Left)
		if err != nil {
			return nil, err
		}
		r, err := g.expr(x.Right)
		if err != nil {
			return nil, err
		}
		if l == nil || r == nil {
			return nil, wireErrf("binary %q needs two operands", x.Op)
		}
		return lir.Binary{Op: lir.BinaryOp(x.Op), L: l, R: r}, nil

	case *lirwire.CastExpr:
		sub, err := g.expr(x.Expr)
		if err != nil {
			return nil, err
		}
		if sub == nil {
			return nil, wireErrf("cast needs an operand")
		}
		return lir.Cast{X: sub, To: lir.Kind(x.To)}, nil

	case *lirwire.BranchExpr:
		if len(x.Branches) == 0 {
			return nil, wireErrf("branch needs at least one arm")
		}
		arms := make([]lir.BranchArm, len(x.Branches))
		for i, arm := range x.Branches {
			when, err := g.expr(arm.When)
			if err != nil {
				return nil, err
			}
			then, err := g.expr(arm.Then)
			if err != nil {
				return nil, err
			}
			if when == nil || then == nil {
				return nil, wireErrf("branch arm %d needs a when and a then", i+1)
			}
			arms[i] = lir.BranchArm{When: when, Then: then}
		}
		els, err := g.expr(x.Else)
		if err != nil {
			return nil, err
		}
		if els == nil {
			return nil, wireErrf("branch needs an else")
		}
		return lir.Branch{Arms: arms, Else: els}, nil

	case *lirwire.TextMatchExpr:
		value, err := g.expr(x.Value)
		if err != nil {
			return nil, err
		}
		if value == nil {
			return nil, wireErrf("text_match needs a value")
		}
		if len(x.Parts) == 0 {
			return nil, wireErrf("text_match needs at least one pattern part")
		}
		parts := make([]lir.TextMatchPart, len(x.Parts))
		for i, part := range x.Parts {
			switch p := part.TextMatchExprPartUnion.(type) {
			case *lirwire.LiteralTextMatchPart:
				parts[i] = lir.LiteralPart{Value: p.Value}
			case *lirwire.AnyManyTextMatchPart:
				parts[i] = lir.AnyManyPart{}
			default:
				return nil, wireErrf("text_match part %d has an unknown kind", i+1)
			}
		}
		comparison := lir.TextComparisonExact
		if x.Comparison != nil {
			comparison = lir.TextComparison(*x.Comparison)
		}
		return lir.TextMatch{Value: value, Parts: parts, Comparison: comparison}, nil

	case *lirwire.CrossingExprExists:
		rel, err := g.rel(x.Node)
		if err != nil {
			return nil, err
		}
		return lir.Exists{Rel: rel}, nil

	case *lirwire.CrossingExprFirst:
		rel, err := g.rel(x.Node)
		if err != nil {
			return nil, err
		}
		return lir.First{Rel: rel}, nil

	case *lirwire.CrossingExprScalar:
		rel, err := g.rel(x.Node)
		if err != nil {
			return nil, err
		}
		return lir.Scalar{Rel: rel}, nil

	case *lirwire.CrossingExprArray:
		rel, err := g.rel(x.Node)
		if err != nil {
			return nil, err
		}
		return lir.Array{Rel: rel}, nil

	default:
		return nil, wireErrf("unknown expression kind %q", e.ExprType())
	}
}

// nodeKind reports a node's discriminator for error messages, tolerating a nil
// union (an empty Node the schema would already have rejected).
func nodeKind(n lirwire.Node) string {
	if n.NodeUnion == nil {
		return ""
	}
	return n.NodeType()
}

func optString(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}

func optBool(b *bool) bool { return b != nil && *b }
