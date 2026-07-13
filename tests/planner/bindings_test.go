package planner

// Relation bindings over the wire: named relational values, referenced as
// fresh occurrences that all observe one committed bag. The node maps stay
// literal — `qb` only adds the bindings section to the envelope.

import (
	"fmt"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

// qb is q plus a bindings section.
func qb(nodes map[string]protocol.Node, bindings map[string]protocol.Binding, root, cardinality string) protocol.Query {
	return protocol.Query{
		Nodes:    nodes,
		Bindings: bindings,
		Root:     protocol.Root{Node: root, Cardinality: cardinality},
	}
}

// The canonical CTE self-join: per-customer order counts, joined to
// themselves. One definition, two occurrences, diagonal result.
func TestBindingCTESelfJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"per_customer": {Kind: "aggregate", Input: "o", Scope: "e",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "customer_id")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"a": {Kind: "ref", Binding: "expensive", Scope: "a"},
		"b": {Kind: "ref", Binding: "expensive", Scope: "b"},
		"j": {Kind: "join", Left: "a", Right: "b", Join: "inner",
			On: protocol.Eq(protocol.Col("a", "customer_id"), protocol.Col("b", "customer_id"))},
		"sorted": {Kind: "order", Input: "j",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("a", "customer_id")}}},
		"out": {Kind: "project", Input: "sorted", Fields: []protocol.Field{
			{As: "customer", Expr: *protocol.Col("a", "customer_id")},
			{As: "n1", Expr: *protocol.Col("a", "n")},
			{As: "n2", Expr: *protocol.Col("b", "n")},
		}},
	}, map[string]protocol.Binding{
		"expensive": {Node: "per_customer"},
	}, "out", "many")).Equals(`[
		{"customer":"c1","n1":3,"n2":3},
		{"customer":"c2","n1":2,"n2":2},
		{"customer":"c3","n1":1,"n2":1},
		{"customer":"c4","n1":1,"n2":1}
	]`)
}

// Diagonal over an arbitrary binding, end to end over the wire: both
// occurrences observe the same committed two-product bag.
func TestBindingDiagonalOverWire(t *testing.T) {
	t.Parallel()
	d := shop(t)
	r := d.Query(qb(map[string]protocol.Node{
		"p":    {Kind: "scan", Table: "products", Scope: "p"},
		"some": {Kind: "slice", Input: "p", Limit: new(int(2))},
		"a":    {Kind: "ref", Binding: "pair", Scope: "a"},
		"b":    {Kind: "ref", Binding: "pair", Scope: "b"},
		"j": {Kind: "join", Left: "a", Right: "b", Join: "inner",
			On: protocol.Eq(protocol.Col("a", "id"), protocol.Col("b", "id"))},
		"out": {Kind: "project", Input: "j", Fields: []protocol.Field{
			{As: "l", Expr: *protocol.Col("a", "id")},
			{As: "r", Expr: *protocol.Col("b", "id")},
		}},
	}, map[string]protocol.Binding{
		"pair": {Node: "some"},
	}, "out", "many")).Len(2)
	for _, rec := range r.Records {
		if rec["l"] != rec["r"] {
			t.Fatalf("off-diagonal row %v: occurrences observed different bags", rec)
		}
	}
}

// A binding chain: stats over a filtered binding, referenced as the root.
func TestBindingChainOverWire(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"open": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending"))},
		"r": {Kind: "ref", Binding: "pending", Scope: "r"},
		"fold": {Kind: "aggregate", Input: "r",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"root": {Kind: "ref", Binding: "stats", Scope: "s"},
	}, map[string]protocol.Binding{
		"pending": {Node: "open"},
		"stats":   {Node: "fold"},
	}, "root", "exactly_one")).Equals(`[{"n":2}]`)
}

// ── the forest preflight ────────────────────────────────────────────────────

func TestBindingUnusedRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]protocol.Node{
		"o":    {Kind: "scan", Table: "orders", Scope: "o"},
		"dead": {Kind: "scan", Table: "products", Scope: "p"},
	}, map[string]protocol.Binding{
		"unused": {Node: "dead"},
	}, "o", "many")).ExpectError(`binding "unused" is never referenced`)
}

func TestBindingUnknownRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "ref", Binding: "ghost", Scope: "r"},
	}, "r", "many")).ExpectError(`references unknown binding "ghost"`)
}

func TestBindingCycleRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The binding's own tree references itself.
	d.Query(qb(map[string]protocol.Node{
		"self": {Kind: "ref", Binding: "loop", Scope: "s"},
		"more": {Kind: "filter", Input: "self",
			Predicate: protocol.Eq(protocol.Col("s", "id"), protocol.Lit("x"))},
		"r": {Kind: "ref", Binding: "loop", Scope: "r"},
	}, map[string]protocol.Binding{
		"loop": {Node: "more"},
	}, "r", "many")).ExpectError(`binding "loop" is part of a binding cycle`)
}

func TestBindingRootAlsoConsumedRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The binding's root node is also consumed by an ordinary edge.
	d.Query(qb(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"f": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending"))},
		"r": {Kind: "ref", Binding: "b", Scope: "r"},
	}, map[string]protocol.Binding{
		"b": {Node: "o"},
	}, "f", "many")).ExpectError(`is also consumed by another node`)
}

func TestBindingHiddenScopeOverWire(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"r": {Kind: "ref", Binding: "all", Scope: "r"},
		"leak": {Kind: "filter", Input: "r",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending"))},
	}, map[string]protocol.Binding{
		"all": {Node: "o"},
	}, "leak", "many")).ExpectError(`scope "o" exists but is not visible`)
}

// Nested bindings, each referenced twice, stay linear — the anti-exponential
// guarantee. Depth 6 would be 2^6 subtrees under macro expansion. Each level
// projects its join to one column: a binding's output schema must be unique.
func TestBindingNestedDepthStaysLinear(t *testing.T) {
	t.Parallel()
	d := shop(t)

	nodes := map[string]protocol.Node{
		"base": {Kind: "scan", Table: "orders", Scope: "s0"},
		"ids": {Kind: "project", Input: "base",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("s0", "id")}}},
	}
	bindings := map[string]protocol.Binding{"b0": {Node: "ids"}}
	prev := "b0"
	for i := 1; i <= 6; i++ {
		l, r := fmt.Sprintf("l%d", i), fmt.Sprintf("r%d", i)
		j, p := fmt.Sprintf("j%d", i), fmt.Sprintf("p%d", i)
		nodes[l] = protocol.Node{Kind: "ref", Binding: prev, Scope: l}
		nodes[r] = protocol.Node{Kind: "ref", Binding: prev, Scope: r}
		nodes[j] = protocol.Node{Kind: "join", Left: l, Right: r, Join: "inner",
			On: protocol.Eq(protocol.Col(l, "id"), protocol.Col(r, "id"))}
		nodes[p] = protocol.Node{Kind: "project", Input: j,
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col(l, "id")}}}
		name := fmt.Sprintf("b%d", i)
		bindings[name] = protocol.Binding{Node: p}
		prev = name
	}
	nodes["top"] = protocol.Node{Kind: "ref", Binding: prev, Scope: "top"}
	nodes["fold"] = protocol.Node{Kind: "aggregate", Input: "top",
		Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}}

	// Each self-join on the full PK keeps exactly the 7 orders.
	d.Query(qb(nodes, bindings, "fold", "exactly_one")).Equals(`[{"n":7}]`)
}

// A binding whose body is a raw join has colliding output columns — an
// ill-formed public schema, rejected with the remedy named.
func TestBindingDuplicateOutputRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"j": {Kind: "join", Left: "o", Right: "c", Join: "inner",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"r": {Kind: "ref", Binding: "wide", Scope: "r"},
	}, map[string]protocol.Binding{
		"wide": {Node: "j"},
	}, "r", "many")).ExpectError(`binding "wide" output has duplicate column "id"`)
}
