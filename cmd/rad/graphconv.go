package main

// graphQuery materialises the wire's query graph — string node references —
// into the engine's unbound IR value tree. It is mechanical and needs no
// catalog: kind and operator dispatch, reference resolution, and cycle
// detection. Names, types, and semantics are the binder's job.

import (
	lir "rad/rad/03_lir"

	"rad/protocol"
)

// graphQuery converts a wire query into an unbound lir.Query.
func graphQuery(q protocol.Query) (lir.Query, error) {
	switch q.Root.Cardinality {
	case "many", "first", "exactly_one", "scalar":
	default:
		return lir.Query{}, wireErrf("unknown root cardinality %q", q.Root.Cardinality)
	}
	g := &graphConv{nodes: q.Nodes, building: map[string]bool{}}
	root, err := g.rel(q.Root.Node)
	if err != nil {
		return lir.Query{}, err
	}
	return lir.Query{Root: root, Card: lir.RootCard(q.Root.Cardinality)}, nil
}

type graphConv struct {
	nodes    map[string]protocol.Node
	building map[string]bool // in-progress set: a re-entry is a cycle
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
	if g.building[name] {
		return nil, wireErrf("node %q is part of a cycle", name)
	}
	g.building[name] = true
	defer delete(g.building, name)

	switch n.Kind {
	case "scan":
		return lir.Scan{Table: n.Table, Scope: n.Scope}, nil

	case "filter":
		in, err := g.rel(n.Input)
		if err != nil {
			return nil, err
		}
		pred, err := g.expr(n.Predicate)
		if err != nil {
			return nil, err
		}
		if pred == nil {
			return nil, wireErrf("filter %q needs a predicate", name)
		}
		return lir.Filter{Input: in, Pred: pred}, nil

	case "project":
		in, err := g.rel(n.Input)
		if err != nil {
			return nil, err
		}
		fields := make([]lir.ProjField, len(n.Fields))
		for i, f := range n.Fields {
			e, err := g.expr(&f.Expr)
			if err != nil {
				return nil, err
			}
			fields[i] = lir.ProjField{As: f.As, Expr: e}
		}
		return lir.Project{Input: in, Scope: n.Scope, Spread: n.Spread, Fields: fields}, nil

	case "join":
		l, err := g.rel(n.Left)
		if err != nil {
			return nil, err
		}
		r, err := g.rel(n.Right)
		if err != nil {
			return nil, err
		}
		on, err := g.expr(n.On)
		if err != nil {
			return nil, err
		}
		if on == nil {
			return nil, wireErrf("join %q needs a condition", name)
		}
		return lir.Join{Left: l, Right: r, Kind: lir.JoinKind(n.Join), On: on}, nil

	case "aggregate":
		in, err := g.rel(n.Input)
		if err != nil {
			return nil, err
		}
		groups := make([]lir.GroupTerm, len(n.Groups))
		for i, gt := range n.Groups {
			e, err := g.expr(&gt.Expr)
			if err != nil {
				return nil, err
			}
			groups[i] = lir.GroupTerm{As: gt.As, Expr: e}
		}
		terms := make([]lir.AggTerm, len(n.Aggs))
		for i, a := range n.Aggs {
			arg, err := g.expr(a.Arg)
			if err != nil {
				return nil, err
			}
			terms[i] = lir.AggTerm{Fn: lir.AggFn(a.Fn), Arg: arg, As: a.As}
		}
		return lir.Aggregate{Input: in, Scope: n.Scope, Groups: groups, Terms: terms}, nil

	case "order":
		in, err := g.rel(n.Input)
		if err != nil {
			return nil, err
		}
		terms := make([]lir.OrderTerm, len(n.Terms))
		for i, t := range n.Terms {
			e, err := g.expr(&t.Expr)
			if err != nil {
				return nil, err
			}
			terms[i] = lir.OrderTerm{Expr: e, Desc: t.Desc}
		}
		return lir.Order{Input: in, Terms: terms}, nil

	case "slice":
		in, err := g.rel(n.Input)
		if err != nil {
			return nil, err
		}
		offset := 0
		if n.Offset != nil {
			offset = *n.Offset
		}
		return lir.Slice{Input: in, Offset: offset, Limit: n.Limit}, nil

	default:
		return nil, wireErrf("node %q has unknown kind %q", name, n.Kind)
	}
}

func (g *graphConv) expr(e *protocol.Expr) (lir.Expr, error) {
	if e == nil {
		return nil, nil
	}
	switch e.Kind {
	case "lit":
		return lir.Literal{Raw: e.Value}, nil

	case "col":
		return lir.Column{Scope: e.Scope, Name: e.Column}, nil

	case "unary":
		sub, err := g.expr(e.Expr)
		if err != nil {
			return nil, err
		}
		if sub == nil {
			return nil, wireErrf("unary %q needs an operand", e.Op)
		}
		return lir.Unary{Op: lir.UnaryOp(e.Op), X: sub}, nil

	case "binary":
		l, err := g.expr(e.Left)
		if err != nil {
			return nil, err
		}
		r, err := g.expr(e.Right)
		if err != nil {
			return nil, err
		}
		if l == nil || r == nil {
			return nil, wireErrf("binary %q needs two operands", e.Op)
		}
		return lir.Binary{Op: lir.BinaryOp(e.Op), L: l, R: r}, nil

	case "call":
		args := make([]lir.Expr, len(e.Args))
		for i := range e.Args {
			a, err := g.expr(&e.Args[i])
			if err != nil {
				return nil, err
			}
			args[i] = a
		}
		return lir.Call{Fn: e.Fn, Args: args}, nil

	case "cast":
		sub, err := g.expr(e.Expr)
		if err != nil {
			return nil, err
		}
		if sub == nil {
			return nil, wireErrf("cast needs an operand")
		}
		return lir.Cast{X: sub, To: lir.Kind(e.To)}, nil

	case "exists", "first", "scalar", "array":
		rel, err := g.rel(e.Node)
		if err != nil {
			return nil, err
		}
		switch e.Kind {
		case "exists":
			return lir.Exists{Rel: rel}, nil
		case "first":
			return lir.First{Rel: rel}, nil
		case "scalar":
			return lir.Scalar{Rel: rel}, nil
		default:
			return lir.Array{Rel: rel}, nil
		}

	default:
		return nil, wireErrf("unknown expression kind %q", e.Kind)
	}
}
