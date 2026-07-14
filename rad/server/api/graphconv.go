package api

// graphQuery materialises the wire's query tree — string node references —
// into the engine's unbound IR value tree. It is mechanical and needs no
// catalog: kind and operator dispatch, reference resolution, and cycle
// detection. Names, types, and semantics are the binder's job.

import (
	"fmt"
	"slices"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/protocol"
)

// graphQuery converts a standalone wire query into an unbound lir.Query.
func graphQuery(q protocol.Query) (lir.Query, error) {
	return graphQueryExternal(q, nil)
}

// graphQueryExternal converts a wire query whose `ref` nodes may resolve to
// external bindings — a PIR statement's refs to earlier statement results,
// which are not among its own local bindings. external is the set of those
// names; nil for a standalone query.
func graphQueryExternal(q protocol.Query, external map[string]bool) (lir.Query, error) {
	switch q.Root.Cardinality {
	case "many", "first", "exactly_one", "scalar":
	default:
		return lir.Query{}, wireErrf("unknown root cardinality %q", q.Root.Cardinality)
	}
	if err := validateWireShapes(q); err != nil {
		return lir.Query{}, err
	}
	if err := validateRelationForest(q, external); err != nil {
		return lir.Query{}, err
	}
	g := &graphConv{nodes: q.Nodes, building: map[string]bool{}}
	root, err := g.rel(q.Root.Node)
	if err != nil {
		return lir.Query{}, err
	}
	var bindings map[string]lir.Relation
	if len(q.Bindings) > 0 {
		bindings = make(map[string]lir.Relation, len(q.Bindings))
		for name, b := range q.Bindings {
			body, err := g.rel(b.Node)
			if err != nil {
				return lir.Query{}, err
			}
			bindings[name] = body
		}
	}
	return lir.Query{Root: root, Card: lir.RootCard(q.Root.Cardinality), Bindings: bindings}, nil
}

// validateWireShapes mirrors the schema's structural checks for callers that
// construct the ergonomic Go graph directly. HTTP requests have already been
// checked by the independent JSON Schema before reaching graph materialisation.
func validateWireShapes(q protocol.Query) error {
	names := make([]string, 0, len(q.Nodes))
	for name := range q.Nodes {
		names = append(names, name)
	}
	slices.Sort(names)
	for _, name := range names {
		n := q.Nodes[name]
		missing := func(field string) error {
			return wireErrf("node %q (%s) needs %s", name, n.Kind, field)
		}
		switch n.Kind {
		case "scan":
			if n.Table == "" {
				return missing("table")
			}
			if n.Scope == "" {
				return missing("scope")
			}
		case "rows":
			if n.Scope == "" {
				return missing("scope")
			}
			if len(n.Columns) == 0 {
				return wireErrf("node %q (rows) needs at least one column", name)
			}
			for i, c := range n.Columns {
				if c.Name == "" {
					return wireErrf("node %q rows column %d needs a name", name, i)
				}
				switch c.Type {
				case "text", "int64", "float64", "bool":
				default:
					return wireErrf("node %q rows column %q has unknown type %q", name, c.Name, c.Type)
				}
			}
			for i, row := range n.Rows {
				if len(row) != len(n.Columns) {
					return wireErrf("node %q rows row %d has %d values, want %d", name, i, len(row), len(n.Columns))
				}
			}
		case "filter":
			if n.Input == "" {
				return missing("input")
			}
			if err := validateWireExpr(n.Predicate, fmt.Sprintf("node %q predicate", name)); err != nil {
				return err
			}
		case "project":
			if n.Input == "" {
				return missing("input")
			}
			if len(n.Spread) == 0 && len(n.Fields) == 0 {
				return wireErrf("node %q (project) needs at least one spread scope or field", name)
			}
			for i := range n.Fields {
				if n.Fields[i].As == "" {
					return wireErrf("node %q project field %d needs as", name, i)
				}
				if err := validateWireExpr(&n.Fields[i].Expr, fmt.Sprintf("node %q project field %q", name, n.Fields[i].As)); err != nil {
					return err
				}
			}
		case "join":
			if n.Left == "" || n.Right == "" {
				return missing("left and right")
			}
			if n.Join != "inner" && n.Join != "left" {
				return wireErrf("node %q has unknown join kind %q", name, n.Join)
			}
			if err := validateWireExpr(n.On, fmt.Sprintf("node %q join condition", name)); err != nil {
				return err
			}
		case "aggregate":
			if n.Input == "" {
				return missing("input")
			}
			if len(n.Groups) == 0 && len(n.Aggs) == 0 {
				return wireErrf("node %q (aggregate) needs at least one group or aggregate", name)
			}
			for i := range n.Groups {
				if err := validateWireExpr(&n.Groups[i].Expr, fmt.Sprintf("node %q group %d", name, i)); err != nil {
					return err
				}
			}
			for i := range n.Aggs {
				if n.Aggs[i].As == "" {
					return wireErrf("node %q aggregate %d needs as", name, i)
				}
				switch n.Aggs[i].Fn {
				case "count", "sum", "avg", "min", "max":
				default:
					return wireErrf("node %q has unknown aggregate function %q", name, n.Aggs[i].Fn)
				}
				if n.Aggs[i].Arg != nil {
					if err := validateWireExpr(n.Aggs[i].Arg, fmt.Sprintf("node %q aggregate %q", name, n.Aggs[i].As)); err != nil {
						return err
					}
				}
			}
		case "order":
			if n.Input == "" {
				return missing("input")
			}
			if len(n.Terms) == 0 {
				return wireErrf("node %q (order) needs at least one term", name)
			}
			for i := range n.Terms {
				if err := validateWireExpr(&n.Terms[i].Expr, fmt.Sprintf("node %q order term %d", name, i)); err != nil {
					return err
				}
			}
		case "slice":
			if n.Input == "" {
				return missing("input")
			}
			if n.Offset != nil && *n.Offset < 0 {
				return wireErrf("node %q slice offset must be >= 0", name)
			}
			if n.Limit != nil && *n.Limit < 0 {
				return wireErrf("node %q slice limit must be >= 0", name)
			}
		case "ref":
			if n.Binding == "" {
				return missing("binding")
			}
			if n.Scope == "" {
				return missing("scope")
			}
		default:
			return wireErrf("node %q has unknown kind %q", name, n.Kind)
		}
	}
	return nil
}

func validateWireExpr(e *protocol.Expr, where string) error {
	if e == nil {
		return wireErrf("%s needs an expression", where)
	}
	switch e.Kind {
	case "lit":
		return nil // nil is the valid NULL literal
	case "col":
		if e.Scope == "" || e.Column == "" {
			return wireErrf("%s column needs scope and column", where)
		}
	case "unary":
		switch e.Op {
		case "not", "negate", "is_null", "is_not_null":
		default:
			return wireErrf("%s has unknown unary operator %q", where, e.Op)
		}
		return validateWireExpr(e.Expr, where+" unary operand")
	case "binary":
		switch e.Op {
		case "eq", "ne", "lt", "lte", "gt", "gte", "and", "or", "add", "sub", "mul", "div":
		default:
			return wireErrf("%s has unknown binary operator %q", where, e.Op)
		}
		if err := validateWireExpr(e.Left, where+" left operand"); err != nil {
			return err
		}
		return validateWireExpr(e.Right, where+" right operand")
	case "cast":
		switch e.To {
		case "text", "int64", "float64", "bool":
		default:
			return wireErrf("%s has unknown cast target %q", where, e.To)
		}
		return validateWireExpr(e.Expr, where+" cast operand")
	case "exists", "first", "scalar", "array":
		if e.Node == "" {
			return wireErrf("%s crossing needs node", where)
		}
	default:
		return wireErrf("%s has unknown expression kind %q", where, e.Kind)
	}
	return nil
}

// validateRelationForest proves that the flat node map encodes a finite,
// single-consumer relation forest: one tree rooted at root.node plus one
// tree per binding root. Ordinary node ids carry no sharing identity —
// every node has exactly one consumer (its parent edge, or the root/binding
// declaration that names it). References are edges into the binding
// namespace, never node-consumer edges; sharing exists only there, and the
// binding dependency graph must itself be acyclic with every binding used.
func validateRelationForest(q protocol.Query, external map[string]bool) error {
	if q.Root.Node == "" {
		return wireErrf("missing root node reference")
	}
	if _, ok := q.Nodes[q.Root.Node]; !ok {
		return wireErrf("unknown node %q", q.Root.Node)
	}

	bindingNames := make([]string, 0, len(q.Bindings))
	for name := range q.Bindings {
		bindingNames = append(bindingNames, name)
	}
	slices.Sort(bindingNames)
	for _, name := range bindingNames {
		b := q.Bindings[name]
		if b.Node == "" {
			return wireErrf("binding %q is missing its defining node", name)
		}
		if _, ok := q.Nodes[b.Node]; !ok {
			return wireErrf("binding %q names unknown node %q", name, b.Node)
		}
	}

	const (
		visiting = 1
		visited  = 2
	)
	state := map[string]uint8{}
	consumers := map[string]int{}
	reachable := map[string]bool{}
	refUses := map[string]int{}
	var currentTree string // "" = the main tree; else the binding being walked
	bindingDeps := map[string][]string{}

	var visit func(string) error
	visit = func(name string) error {
		if name == "" {
			return wireErrf("missing node reference")
		}
		n, ok := q.Nodes[name]
		if !ok {
			return wireErrf("unknown node %q", name)
		}
		switch state[name] {
		case visiting:
			return wireErrf("node %q is part of a cycle", name)
		case visited:
			return nil
		}
		state[name] = visiting
		reachable[name] = true
		if n.Kind == "ref" {
			_, isLocal := q.Bindings[n.Binding]
			switch {
			case isLocal:
				refUses[n.Binding]++
				if currentTree != "" {
					bindingDeps[currentTree] = append(bindingDeps[currentTree], n.Binding)
				}
			case external[n.Binding]:
				// Resolves to an external (program) binding — an earlier PIR
				// statement's result, neither a local binding to count nor a
				// dependency to order here. The planner resolves it at bind
				// time.
			default:
				return wireErrf("node %q references unknown binding %q", name, n.Binding)
			}
		}
		for _, ref := range relationRefs(n) {
			if ref == "" {
				return wireErrf("node %q has a missing node reference", name)
			}
			consumers[ref]++
			if err := visit(ref); err != nil {
				return err
			}
			if consumers[ref] > 1 {
				return wireErrf("node %q has multiple consumers; LIR requires a single-consumer relation forest — name it as a binding to share it", ref)
			}
		}
		state[name] = visited
		return nil
	}

	// Tree roots must be distinct: root.node and every binding root each
	// head exactly one tree.
	roots := map[string]string{q.Root.Node: "the query root"}
	for _, name := range bindingNames {
		bn := q.Bindings[name].Node
		if prev, taken := roots[bn]; taken {
			return wireErrf("node %q roots both %s and binding %q", bn, prev, name)
		}
		roots[bn] = fmt.Sprintf("binding %q", name)
	}

	if err := visit(q.Root.Node); err != nil {
		return err
	}
	for _, name := range bindingNames {
		currentTree = name
		if err := visit(q.Bindings[name].Node); err != nil {
			return err
		}
	}
	currentTree = ""

	for root, owner := range roots {
		if consumers[root] != 0 {
			return wireErrf("node %q heads %s and is also consumed by another node", root, owner)
		}
	}
	for _, name := range bindingNames {
		if refUses[name] == 0 {
			return wireErrf("binding %q is never referenced", name)
		}
	}
	if err := checkBindingCycles(bindingNames, bindingDeps); err != nil {
		return err
	}
	if len(reachable) != len(q.Nodes) {
		var unused []string
		for name := range q.Nodes {
			if !reachable[name] {
				unused = append(unused, name)
			}
		}
		slices.Sort(unused)
		return wireErrf("unreachable node definitions: %s", fmt.Sprint(unused))
	}
	return nil
}

// checkBindingCycles rejects cyclic dependencies among bindings (a binding
// whose tree references itself, directly or through other bindings).
// Recursion is a future construct with its own rules, never an accident.
func checkBindingCycles(names []string, deps map[string][]string) error {
	const (
		visiting = 1
		visited  = 2
	)
	state := map[string]uint8{}
	var visit func(string) error
	visit = func(name string) error {
		switch state[name] {
		case visiting:
			return wireErrf("binding %q is part of a binding cycle", name)
		case visited:
			return nil
		}
		state[name] = visiting
		for _, dep := range deps[name] {
			if err := visit(dep); err != nil {
				return err
			}
		}
		state[name] = visited
		return nil
	}
	for _, name := range names {
		if err := visit(name); err != nil {
			return err
		}
	}
	return nil
}

func relationRefs(n protocol.Node) []string {
	var refs []string
	switch n.Kind {
	case "filter":
		refs = append(refs, n.Input)
		appendExprRefs(&refs, n.Predicate)
	case "project":
		refs = append(refs, n.Input)
		for i := range n.Fields {
			appendExprRefs(&refs, &n.Fields[i].Expr)
		}
	case "join":
		refs = append(refs, n.Left, n.Right)
		appendExprRefs(&refs, n.On)
	case "aggregate":
		refs = append(refs, n.Input)
		for i := range n.Groups {
			appendExprRefs(&refs, &n.Groups[i].Expr)
		}
		for i := range n.Aggs {
			appendExprRefs(&refs, n.Aggs[i].Arg)
		}
	case "order":
		refs = append(refs, n.Input)
		for i := range n.Terms {
			appendExprRefs(&refs, &n.Terms[i].Expr)
		}
	case "slice":
		refs = append(refs, n.Input)
	}
	return refs
}

func appendExprRefs(refs *[]string, e *protocol.Expr) {
	if e == nil {
		return
	}
	switch e.Kind {
	case "unary", "cast":
		appendExprRefs(refs, e.Expr)
	case "binary":
		appendExprRefs(refs, e.Left)
		appendExprRefs(refs, e.Right)
	case "exists", "first", "scalar", "array":
		*refs = append(*refs, e.Node)
	}
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

	case "rows":
		cols := make([]lir.RowsCol, len(n.Columns))
		for i, c := range n.Columns {
			cols[i] = lir.RowsCol{Name: c.Name, Kind: lir.Kind(c.Type), Nullable: c.Nullable}
		}
		return lir.Rows{Scope: n.Scope, Columns: cols, Values: n.Rows}, nil

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

	case "ref":
		return lir.Ref{Binding: n.Binding, Scope: n.Scope}, nil

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
