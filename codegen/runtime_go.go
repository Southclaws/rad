package codegen

// goQueryRuntime is emitted verbatim into every generated Go client: the
// spec types the fluent builders accumulate, and the assembler that compiles
// a spec into the wire's query graph with deterministic node ids (a preorder
// walk; each scan's id doubles as its scope label).
const goQueryRuntime = `// querySpec accumulates a fluent builder's state; assemble compiles it into
// the wire's query graph.
type querySpec struct {
	table    string
	filters  []*protocol.Expr
	orders   []protocol.OrderTerm
	offset   int
	limit    int
	limitSet bool
	includes []includeSpec
	aggs     []protocol.AggTerm
}

// includeSpec is one relation field: a correlated sub-query materialised as
// a nested object (first), array, or folded scalar object.
type includeSpec struct {
	table    string
	pairs    [][2]string // scanned-side column = outer-scope column
	as       string
	kind     string // first | array | fold
	filters  []*protocol.Expr
	orders   []protocol.OrderTerm
	limit    int
	limitSet bool
	nested   []includeSpec
	aggs     []protocol.AggTerm
}

func assemble(s querySpec) protocol.Query {
	g := &graphBuilder{nodes: map[string]protocol.Node{}}
	last, scope := g.chain(s.table, nil, "", s.filters, s.orders, s.offset, s.limit, s.limitSet)
	if len(s.aggs) > 0 {
		id := g.next()
		g.nodes[id] = protocol.Node{Kind: "aggregate", Input: last, Aggs: scopeAggs(s.aggs, scope)}
		return protocol.Query{Nodes: g.nodes, Root: protocol.Root{Node: id, Cardinality: "exactly_one"}}
	}
	if len(s.includes) > 0 {
		fields := make([]protocol.Field, len(s.includes))
		for i, inc := range s.includes {
			fields[i] = g.include(inc, scope)
		}
		id := g.next()
		g.nodes[id] = protocol.Node{Kind: "project", Input: last, Spread: []string{scope}, Fields: fields}
		last = id
	}
	return protocol.Query{Nodes: g.nodes, Root: protocol.Root{Node: last, Cardinality: "many"}}
}

type graphBuilder struct {
	nodes map[string]protocol.Node
	n     int
}

func (g *graphBuilder) next() string {
	id := fmt.Sprintf("n%d", g.n)
	g.n++
	return id
}

// chain emits scan → filter → order → slice as needed and returns the last
// node id plus the scan's scope. pairs prepend correlation equalities
// against outerScope.
func (g *graphBuilder) chain(table string, pairs [][2]string, outerScope string, filters []*protocol.Expr, orders []protocol.OrderTerm, offset, limit int, limitSet bool) (string, string) {
	scope := g.next()
	g.nodes[scope] = protocol.Node{Kind: "scan", Table: table, Scope: scope}
	last := scope

	var preds []*protocol.Expr
	for _, pr := range pairs {
		preds = append(preds, protocol.Eq(protocol.Col(scope, pr[0]), protocol.Col(outerScope, pr[1])))
	}
	for _, f := range filters {
		preds = append(preds, scopeExpr(f, scope))
	}
	if pred := protocol.AndAll(preds); pred != nil {
		id := g.next()
		g.nodes[id] = protocol.Node{Kind: "filter", Input: last, Predicate: pred}
		last = id
	}
	if len(orders) > 0 {
		terms := make([]protocol.OrderTerm, len(orders))
		for i, t := range orders {
			terms[i] = protocol.OrderTerm{Expr: *scopeExpr(&t.Expr, scope), Desc: t.Desc}
		}
		id := g.next()
		g.nodes[id] = protocol.Node{Kind: "order", Input: last, Terms: terms}
		last = id
	}
	if offset > 0 || limitSet {
		node := protocol.Node{Kind: "slice", Input: last}
		if offset > 0 {
			o := offset
			node.Offset = &o
		}
		if limitSet {
			l := limit
			node.Limit = &l
		}
		id := g.next()
		g.nodes[id] = node
		last = id
	}
	return last, scope
}

func (g *graphBuilder) include(inc includeSpec, outerScope string) protocol.Field {
	if inc.kind == "fold" {
		last, scope := g.chain(inc.table, inc.pairs, outerScope, inc.filters, nil, 0, 0, false)
		id := g.next()
		g.nodes[id] = protocol.Node{Kind: "aggregate", Input: last, Aggs: scopeAggs(inc.aggs, scope)}
		return protocol.Field{As: inc.as, Expr: *protocol.FirstOf(id)}
	}
	last, scope := g.chain(inc.table, inc.pairs, outerScope, inc.filters, inc.orders, 0, inc.limit, inc.limitSet)
	if len(inc.nested) > 0 {
		fields := make([]protocol.Field, len(inc.nested))
		for i, n := range inc.nested {
			fields[i] = g.include(n, scope)
		}
		id := g.next()
		g.nodes[id] = protocol.Node{Kind: "project", Input: last, Spread: []string{scope}, Fields: fields}
		last = id
	}
	if inc.kind == "first" {
		return protocol.Field{As: inc.as, Expr: *protocol.FirstOf(last)}
	}
	return protocol.Field{As: inc.as, Expr: *protocol.ArrayOf(last)}
}

// scopeExpr fills unscoped column references with the relation's scope.
func scopeExpr(e *protocol.Expr, scope string) *protocol.Expr {
	if e == nil {
		return nil
	}
	c := *e
	if c.Kind == "col" && c.Scope == "" {
		c.Scope = scope
	}
	c.Expr = scopeExpr(c.Expr, scope)
	c.Left = scopeExpr(c.Left, scope)
	c.Right = scopeExpr(c.Right, scope)
	if len(c.Args) > 0 {
		args := make([]protocol.Expr, len(c.Args))
		for i := range c.Args {
			args[i] = *scopeExpr(&c.Args[i], scope)
		}
		c.Args = args
	}
	return &c
}

func scopeAggs(aggs []protocol.AggTerm, scope string) []protocol.AggTerm {
	out := make([]protocol.AggTerm, len(aggs))
	for i, a := range aggs {
		out[i] = protocol.AggTerm{Fn: a.Fn, Arg: scopeExpr(a.Arg, scope), As: a.As}
	}
	return out
}
`
