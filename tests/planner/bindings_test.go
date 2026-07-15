package planner

// Relation bindings over the wire: named relational values, referenced as
// fresh occurrences that all observe one committed bag. The node maps stay
// literal — `qb` only adds the bindings section to the envelope.

import (
	"fmt"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// qb is q plus a bindings section.
func qb(nodes map[string]lirwire.Node, bindings map[string]lirwire.Binding, root, cardinality string) lirwire.Query {
	for name, node := range nodes {
		walkNode(nodes, name, &node)
		nodes[name] = node
	}
	root = observableRoot(nodes, root, cardinality)
	return lirwire.Query{
		Nodes:    nodes,
		Bindings: bindings,
		Root:     lirwire.Root{Node: root, Cardinality: cardinality},
	}
}

// The canonical CTE self-join: per-customer order counts, joined to
// themselves. One definition, two occurrences, diagonal result.
func TestBindingCTESelfJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"per_customer": lirwire.Aggregate("o", "e",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "customer_id")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"a": lirwire.Ref("expensive", "a"),
		"b": lirwire.Ref("expensive", "b"),
		"j": lirwire.Join("a", "b", "inner",
			lirwire.Binary("eq", lirwire.Col("a", "customer_id"), lirwire.Col("b", "customer_id"))),
		"sorted": lirwire.Order("j",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("a", "customer_id")}}),
		"out": lirwire.Project("sorted", "", nil, []lirwire.Field{
			{As: "customer", Expr: lirwire.Col("a", "customer_id")},
			{As: "n1", Expr: lirwire.Col("a", "n")},
			{As: "n2", Expr: lirwire.Col("b", "n")},
		}),
	}, map[string]lirwire.Binding{
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
	r := d.Query(qb(map[string]lirwire.Node{
		"p":    lirwire.Scan("products", "p"),
		"some": lirwire.Slice("p", 0, ptrInt(2)),
		"a":    lirwire.Ref("pair", "a"),
		"b":    lirwire.Ref("pair", "b"),
		"j": lirwire.Join("a", "b", "inner",
			lirwire.Binary("eq", lirwire.Col("a", "id"), lirwire.Col("b", "id"))),
		"out": lirwire.Project("j", "", nil, []lirwire.Field{
			{As: "l", Expr: lirwire.Col("a", "id")},
			{As: "r", Expr: lirwire.Col("b", "id")},
		}),
	}, map[string]lirwire.Binding{
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
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"open": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))),
		"r": lirwire.Ref("pending", "r"),
		"fold": lirwire.Aggregate("r", "",
			nil, []lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"root": lirwire.Ref("stats", "s"),
	}, map[string]lirwire.Binding{
		"pending": {Node: "open"},
		"stats":   {Node: "fold"},
	}, "root", "exactly_one")).Equals(`[{"n":2}]`)
}

// -
// the forest preflight
// -

// A binding no ref observes denotes nothing — rejected by the binder, which
// tracks which bindings a ref resolves.
func TestBindingUnusedRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"o":    lirwire.Scan("orders", "o"),
		"dead": lirwire.Scan("products", "p"),
	}, map[string]lirwire.Binding{
		"unused": {Node: "dead"},
	}, "o", "many")).ExpectError(`binding "unused" is never referenced`)
}

func TestBindingUnknownRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"r": lirwire.Ref("ghost", "r"),
	}, "r", "many")).ExpectError(`unknown binding "ghost"`)
}

func TestBindingCycleRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The binding's own tree references itself.
	d.Query(qb(map[string]lirwire.Node{
		"self": lirwire.Ref("loop", "s"),
		"more": lirwire.Filter("self",
			lirwire.Binary("eq", lirwire.Col("s", "id"), lirwire.LitOf("x"))),
		"r": lirwire.Ref("loop", "r"),
	}, map[string]lirwire.Binding{
		"loop": {Node: "more"},
	}, "r", "many")).ExpectError(`binding "loop" is part of a binding cycle`)
}

func TestBindingRootAlsoConsumedRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The binding's root node "o" is also consumed directly by an ordinary edge
	// ("f"), not through a ref — so it is lowered twice under one scope. Sharing
	// a relation is exactly what a binding+ref is for; consuming its root
	// directly as well collides. ("r" keeps the binding referenced and every
	// node reachable, isolating the collision from the unused/orphan rules.)
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"f": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))),
		"r": lirwire.Ref("b", "r"),
		"j": lirwire.Join("f", "r", "inner", lirwire.LitOf(true)),
	}, map[string]lirwire.Binding{
		"b": {Node: "o"},
	}, "j", "many")).ExpectError(`duplicate scope`)
}

func TestBindingHiddenScopeOverWire(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"r": lirwire.Ref("all", "r"),
		"leak": lirwire.Filter("r",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))),
	}, map[string]lirwire.Binding{
		"all": {Node: "o"},
	}, "leak", "many")).ExpectError(`scope "o" exists but is not visible`)
}

// Nested bindings, each referenced twice, stay linear — the anti-exponential
// guarantee. Depth 6 would be 2^6 subtrees under macro expansion. Each level
// projects its join to one column: a binding's output schema must be unique.
func TestBindingNestedDepthStaysLinear(t *testing.T) {
	t.Parallel()
	d := shop(t)

	nodes := map[string]lirwire.Node{
		"base": lirwire.Scan("orders", "s0"),
		"ids": lirwire.Project("base", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("s0", "id")}}),
	}
	bindings := map[string]lirwire.Binding{"b0": {Node: "ids"}}
	prev := "b0"
	for i := 1; i <= 6; i++ {
		l, r := fmt.Sprintf("l%d", i), fmt.Sprintf("r%d", i)
		j, p := fmt.Sprintf("j%d", i), fmt.Sprintf("p%d", i)
		nodes[l] = lirwire.Ref(prev, l)
		nodes[r] = lirwire.Ref(prev, r)
		nodes[j] = lirwire.Join(l, r, "inner",
			lirwire.Binary("eq", lirwire.Col(l, "id"), lirwire.Col(r, "id")))
		nodes[p] = lirwire.Project(j, "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col(l, "id")}})
		name := fmt.Sprintf("b%d", i)
		bindings[name] = lirwire.Binding{Node: p}
		prev = name
	}
	nodes["top"] = lirwire.Ref(prev, "top")
	nodes["fold"] = lirwire.Aggregate("top", "",
		nil, []lirwire.AggTerm{{Fn: "count", As: "n"}})

	// Each self-join on the full PK keeps exactly the 7 orders.
	d.Query(qb(nodes, bindings, "fold", "exactly_one")).Equals(`[{"n":7}]`)
}

// A binding whose body is a raw join has colliding output columns — an
// ill-formed public schema, rejected with the remedy named.
func TestBindingDuplicateOutputRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"c": lirwire.Scan("customers", "c"),
		"j": lirwire.Join("o", "c", "inner",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"r": lirwire.Ref("wide", "r"),
	}, map[string]lirwire.Binding{
		"wide": {Node: "j"},
	}, "r", "many")).ExpectError(`binding "wide" output has duplicate column "id"`)
}

// -
// validation hardening
// -

func TestBindingEmptyBindingsRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	status, body := postQuery(t, d, `{
		"nodes": {"o": {"kind": "scan", "table": "orders", "scope": "o"}},
		"bindings": {},
		"root": {"node": "o", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("empty bindings: status %d, want rejection\n%s", status, body)
	}
	if !strings.Contains(body, "bindings must not be empty when present") {
		t.Fatalf("rejection does not name the rule:\n%s", body)
	}
}

func TestBindingMalformedValueNamed(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// An empty node string inside a binding value: the failure names the
	// binding, not a raw validator path.
	status, body := postQuery(t, d, `{
		"nodes": {
			"o": {"kind": "scan", "table": "orders", "scope": "o"},
			"r": {"kind": "ref", "binding": "x", "scope": "r"}
		},
		"bindings": {"x": {"node": ""}},
		"root": {"node": "r", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("malformed binding: status %d, want rejection\n%s", status, body)
	}
	if !strings.Contains(body, `binding \"x\"`) {
		t.Fatalf("rejection does not name the binding:\n%s", body)
	}
}

// An alias binding — a binding whose root node is itself a ref — is legal:
// it denotes the same committed value under another name.
func TestBindingAlias(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"open": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))),
		"alias_root": lirwire.Ref("base", "ar"),
		"use":        lirwire.Ref("alias", "u"),
		"fold": lirwire.Aggregate("use", "",
			nil, []lirwire.AggTerm{{Fn: "count", As: "n"}}),
	}, map[string]lirwire.Binding{
		"base":  {Node: "open"},
		"alias": {Node: "alias_root"},
	}, "fold", "exactly_one")).Equals(`[{"n":2}]`)
}

// Two references may not share a scope label — same query-wide rule as scans.
func TestBindingDuplicateRefScopeRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"x": lirwire.Ref("b", "same"),
		"y": lirwire.Ref("b", "same"),
		"j": lirwire.Join("x", "y", "inner",
			lirwire.Binary("eq", lirwire.Col("same", "id"), lirwire.Col("same", "id"))),
	}, map[string]lirwire.Binding{
		"b": {Node: "o"},
	}, "j", "many")).ExpectError(`duplicate scope "same"`)
}
