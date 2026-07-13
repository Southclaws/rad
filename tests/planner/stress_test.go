package planner

// Compositional-depth and adversarial cases: deep operator chains, crossings
// containing joins, aggregates of aggregates, and the preflight's structural
// rejections (cycles, sharing, dangling refs, unreachable nodes). Every
// expectation is hand-derived from the fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

// 1. Three projections stacked, each renaming the previous one's output via
// its scope label. The filter sits below everything.
func TestStressTripleProjectionChain(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"gold": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "tier"), protocol.Lit("gold"))},
		"a": {Kind: "project", Input: "gold", Scope: "a",
			Fields: []protocol.Field{{As: "n", Expr: *protocol.Col("c", "name")}}},
		"b": {Kind: "project", Input: "a", Scope: "b",
			Fields: []protocol.Field{{As: "m", Expr: *protocol.Col("a", "n")}}},
		"out": {Kind: "project", Input: "b",
			Fields: []protocol.Field{{As: "name", Expr: *protocol.Col("b", "m")}}},
	}, "out", "many")).Equals(`[{"name":"Ada"},{"name":"Cyn"}]`)
}

// 2. Order, project, order again — the LAST order wins. placed_at desc would
// give o7-first; the re-order by v.id asc restores o1..o7.
func TestStressOrderProjectOrderSandwich(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"newest": {Kind: "order", Input: "o",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"v": {Kind: "project", Input: "newest", Scope: "v", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "at", Expr: *protocol.Col("o", "placed_at")},
		}},
		"asc": {Kind: "order", Input: "v",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("v", "id")}}},
		"out": {Kind: "project", Input: "asc",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("v", "id")}}},
	}, "out", "many")).Equals(`[
		{"id":"o1"},{"id":"o2"},{"id":"o3"},{"id":"o4"},{"id":"o5"},{"id":"o6"},{"id":"o7"}
	]`)
}

// 3. Filter ABOVE a slice: take the 3 newest orders (o7,o5,o6), THEN keep
// delivered. Only o6 survives — slice-then-filter is not filter-then-slice.
func TestStressFilterAboveSlice(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"newest": {Kind: "order", Input: "o",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"top3": {Kind: "slice", Input: "newest", Limit: new(int(3))},
		"delivered": {Kind: "filter", Input: "top3",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("delivered"))},
		"out": {Kind: "project", Input: "delivered",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o6"}]`)
}

// 4. Aggregate above a slice: fold over exactly the top-3 page (o7,o5,o6),
// not the whole table.
func TestStressAggregateAboveSlice(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"newest": {Kind: "order", Input: "o",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"top3": {Kind: "slice", Input: "newest", Limit: new(int(3))},
		"fold": {Kind: "aggregate", Input: "top3", Aggs: []protocol.AggTerm{
			{Fn: "count", As: "n"},
			{Fn: "max", Arg: protocol.Col("o", "placed_at"), As: "max"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"n":3,"max":3500}]`)
}

// 5. A scalar crossing whose sub-relation contains a JOIN: per gold customer,
// total spend = sum(qty*unit_price) over their orders joined to items.
// c1: o1(130)+o2(300)+o7(120)=550; c3: o5(500)=500.
func TestStressCrossingContainingJoin(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"gold": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "tier"), protocol.Lit("gold"))},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"lines": {Kind: "join", Left: "o", Right: "i", Join: "inner",
			On: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"mine": {Kind: "filter", Input: "lines",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"spend": {Kind: "aggregate", Input: "mine", Aggs: []protocol.AggTerm{
			{Fn: "sum", Arg: &protocol.Expr{Kind: "binary", Op: "mul",
				Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")}, As: "spent"},
		}},
		"out": {Kind: "project", Input: "gold", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "spent", Expr: *protocol.ScalarOf("spend")},
		}},
	}, "out", "many")).Equals(`[{"id":"c1","spent":550},{"id":"c3","spent":500}]`)
}

// 6. Join whose LEFT side is a labelled projection that itself carries a
// scalar crossing (per-customer order count), joined against a plain orders
// scan, filtered to pending above the join.
func TestStressJoinOfProjectionsWithCrossing(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c":  {Kind: "scan", Table: "customers", Scope: "c"},
		"co": {Kind: "scan", Table: "orders", Scope: "co"},
		"mine": {Kind: "filter", Input: "co",
			Predicate: protocol.Eq(protocol.Col("co", "customer_id"), protocol.Col("c", "id"))},
		"n": {Kind: "aggregate", Input: "mine",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"cc": {Kind: "project", Input: "c", Scope: "cc", Fields: []protocol.Field{
			{As: "cid", Expr: *protocol.Col("c", "id")},
			{As: "n_orders", Expr: *protocol.ScalarOf("n")},
		}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"j": {Kind: "join", Left: "cc", Right: "o", Join: "inner",
			On: protocol.Eq(protocol.Col("cc", "cid"), protocol.Col("o", "customer_id"))},
		"pending": {Kind: "filter", Input: "j",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending"))},
		"by_order": {Kind: "order", Input: "pending",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_order", Fields: []protocol.Field{
			{As: "cid", Expr: *protocol.Col("cc", "cid")},
			{As: "n", Expr: *protocol.Col("cc", "n_orders")},
			{As: "order", Expr: *protocol.Col("o", "id")},
		}},
	}, "out", "many")).Equals(`[
		{"cid":"c3","n":1,"order":"o5"},
		{"cid":"c1","n":3,"order":"o7"}
	]`)
}

// 7. Six operators in one straight chain. placed_at >= 1500 ascending is
// o3,o4,o2,o6,o5,o7; offset 1 limit 4 keeps o4,o2,o6,o5; the filter above
// the labelled projection drops cancelled o4.
func TestStressSixDeepChain(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"recent": {Kind: "filter", Input: "o",
			Predicate: protocol.Gte(protocol.Col("o", "placed_at"), protocol.Lit(1500))},
		"chrono": {Kind: "order", Input: "recent",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at")}}},
		"page": {Kind: "slice", Input: "chrono", Offset: new(int(1)), Limit: new(int(4))},
		"w": {Kind: "project", Input: "page", Scope: "w", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "status", Expr: *protocol.Col("o", "status")},
		}},
		"live": {Kind: "filter", Input: "w",
			Predicate: protocol.Ne(protocol.Col("w", "status"), protocol.Lit("cancelled"))},
		"out": {Kind: "project", Input: "live",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("w", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o2"},{"id":"o6"},{"id":"o5"}]`)
}

// 8. Four levels of nesting: customer → orders array → items array → product
// object, projecting only the product's name at the deepest level. Item ids
// are TEXT, so "by id" is lexicographic: o7's items order i10 before i9.
func TestStressFourLevelNesting(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1"))},

		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"prod": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "id"), protocol.Col("i", "product_id"))},
		"pname": {Kind: "project", Input: "prod",
			Fields: []protocol.Field{{As: "name", Expr: *protocol.Col("p", "name")}}},

		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"oi": {Kind: "filter", Input: "i",
			Predicate: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"oi_by_id": {Kind: "order", Input: "oi",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("i", "id")}}},
		"item_out": {Kind: "project", Input: "oi_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("i", "id")},
			{As: "product", Expr: *protocol.FirstOf("pname")},
		}},

		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"my_orders": {Kind: "filter", Input: "o",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id")),
				protocol.Ne(protocol.Col("o", "status"), protocol.Lit("cancelled")),
			})},
		"chrono": {Kind: "order", Input: "my_orders",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at")}}},
		"order_out": {Kind: "project", Input: "chrono", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "items", Expr: *protocol.ArrayOf("item_out")},
		}},

		"out": {Kind: "project", Input: "c1", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "orders", Expr: *protocol.ArrayOf("order_out")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","orders":[
			{"id":"o1","items":[
				{"id":"i1","product":{"name":"Keyboard"}},
				{"id":"i2","product":{"name":"Notebook"}}
			]},
			{"id":"o2","items":[
				{"id":"i3","product":{"name":"Monitor"}}
			]},
			{"id":"o7","items":[
				{"id":"i10","product":{"name":"Mouse"}},
				{"id":"i9","product":{"name":"Keyboard"}}
			]}
		]}
	]`)
}

// 9. Top 2 customers by total spend desc, spend computed by a scalar crossing
// over a join. Spends: c2=565, c1=550, c3=500, c4=100, c5=NULL (sum over no
// rows). Per lir.md, Order puts NULLs LAST under desc, so c5 cannot reach the
// top of the ranking — this assertion pins that rule.
func TestStressTopNBySpendCrossing(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"lines": {Kind: "join", Left: "o", Right: "i", Join: "inner",
			On: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"mine": {Kind: "filter", Input: "lines",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"spend": {Kind: "aggregate", Input: "mine", Aggs: []protocol.AggTerm{
			{Fn: "sum", Arg: &protocol.Expr{Kind: "binary", Op: "mul",
				Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")}, As: "spend"},
		}},
		"s": {Kind: "project", Input: "c", Scope: "s", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "spend", Expr: *protocol.ScalarOf("spend")},
		}},
		"ranked": {Kind: "order", Input: "s",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("s", "spend"), Desc: true}}},
		"top2": {Kind: "slice", Input: "ranked", Limit: new(int(2))},
	}, "top2", "many")).Equals(`[
		{"id":"c2","spend":565},
		{"id":"c1","spend":550}
	]`)
}

// 10. Aggregate of an aggregate: per-customer order counts, then the average
// count. FOOTGUN: c5 has no orders, so it never forms a group — the average
// is over the 4 customers who DO have orders: (3+2+1+1)/4 = 1.75, not 7/5.
func TestStressAggregateOfAggregate(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"per": {Kind: "aggregate", Input: "o", Scope: "per",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "customer_id")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"overall": {Kind: "aggregate", Input: "per", Aggs: []protocol.AggTerm{
			{Fn: "avg", Arg: protocol.Col("per", "n"), As: "avg_n"},
		}},
	}, "overall", "exactly_one")).Equals(`[{"avg_n":1.75}]`)
}

// 11. HAVING via a wrapped crossing: keep customers whose delivered-order
// count >= 1. Delivered: c1 (o1), c2 (o3), c4 (o6); c3 and c5 have zero
// (count is never NULL, so gte(0,1) is plain FALSE, not UNKNOWN).
func TestStressWrappedCrossingHaving(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"delivered": {Kind: "filter", Input: "o",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id")),
				protocol.Eq(protocol.Col("o", "status"), protocol.Lit("delivered")),
			})},
		"n": {Kind: "aggregate", Input: "delivered",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"keep": {Kind: "filter", Input: "c",
			Predicate: protocol.Gte(protocol.ScalarOf("n"), protocol.Lit(1))},
		"out": {Kind: "project", Input: "keep",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c4"}]`)
}

// 12. Arithmetic over a column that is itself computed by a labelled
// projection below: total = qty*unit_price, then taxed = total*2.
// o1's items: i1 total 80 → 160, i2 total 50 → 100.
func TestStressArithmeticOverComputedColumn(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"o1_items": {Kind: "filter", Input: "i",
			Predicate: protocol.Eq(protocol.Col("i", "order_id"), protocol.Lit("o1"))},
		"l": {Kind: "project", Input: "o1_items", Scope: "l", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("i", "id")},
			{As: "total", Expr: *protocol.Mul(protocol.Col("i", "quantity"), protocol.Col("i", "unit_price"))},
		}},
		"by_id": {Kind: "order", Input: "l",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("l", "id")}}},
		"out": {Kind: "project", Input: "by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("l", "id")},
			{As: "taxed", Expr: *protocol.Mul(protocol.Col("l", "total"), protocol.Lit(2))},
		}},
	}, "out", "many")).Equals(`[{"id":"i1","taxed":160},{"id":"i2","taxed":100}]`)
}

// 13. Filter above a labelled projection referencing a RENAMED column: the
// original names (email, tier) are gone above "v"; only v.handle and v.t
// exist there.
func TestStressFilterAboveRenamedProjection(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"v": {Kind: "project", Input: "c", Scope: "v", Fields: []protocol.Field{
			{As: "handle", Expr: *protocol.Col("c", "email")},
			{As: "t", Expr: *protocol.Col("c", "tier")},
		}},
		"gold": {Kind: "filter", Input: "v",
			Predicate: protocol.Eq(protocol.Col("v", "t"), protocol.Lit("gold"))},
		"out": {Kind: "project", Input: "gold",
			Fields: []protocol.Field{{As: "handle", Expr: *protocol.Col("v", "handle")}}},
	}, "out", "many")).Equals(`[{"handle":"ada@shop.io"},{"handle":"cyn@shop.io"}]`)
}

// 14. An orphan node nothing references is rejected by the preflight.
func TestStressUnreachableNodeRejected(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"out": {Kind: "project", Input: "c",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
		"orphan": {Kind: "scan", Table: "orders", Scope: "z"},
	}, "out", "many")).ExpectStatus(422).ExpectError("unreachable node definitions")
}

// 15. A reference to a node id that does not exist is rejected.
func TestStressDanglingReferenceRejected(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"bad": {Kind: "filter", Input: "nope",
			Predicate: protocol.Eq(protocol.Lit(1), protocol.Lit(1))},
	}, "bad", "many")).ExpectError(`unknown node "nope"`)
}

// 16. Single-consumer law: one sub-node consumed by TWO crossings in the same
// projection is sharing, and LIR has no sharing semantics.
func TestStressSharedNodeRejected(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"out": {Kind: "project", Input: "c", Fields: []protocol.Field{
			{As: "has_any", Expr: *protocol.Exists("mine")},
			{As: "has_any_again", Expr: *protocol.Exists("mine")},
		}},
	}, "out", "many")).ExpectError("multiple consumers")
}

// 17. Two filters consuming each other form a wire cycle; value nodes cannot
// mean anything cyclic, so the graph decoder rejects it outright.
func TestStressCycleRejected(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"a": {Kind: "filter", Input: "b",
			Predicate: protocol.Eq(protocol.Lit(1), protocol.Lit(1))},
		"b": {Kind: "filter", Input: "a",
			Predicate: protocol.Eq(protocol.Lit(1), protocol.Lit(1))},
	}, "a", "many")).ExpectError("part of a cycle")
}

// 18. Root cardinality "first" over an ordered relation: exactly one record,
// which the wire (and thus the harness) sees as a single-element record list.
func TestStressRootFirst(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"newest": {Kind: "order", Input: "o",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
	}, "newest", "first")).Equals(`[
		{"id":"o7","customer_id":"c1","status":"pending","placed_at":3500,"discount":null}
	]`)
}

// 19. exactly_one over a multi-row relation is a runtime cardinality
// violation. It surfaces as an "exec:" engine error, which the server relays
// as a 422 client problem — not a 500.
func TestStressExactlyOneViolation(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
	}, "o", "exactly_one")).ExpectStatus(422).ExpectCode("execution_failed").ExpectError("exactly one")
}

// 20. Pagination disjointness: the same total order (placed_at asc, id
// tiebreak) sliced at [0,3) and [3,6) yields disjoint, exactly-adjacent
// pages. Ascending placed_at: o1,o3,o4,o2,o6,o5,o7.
func TestStressPaginationDisjoint(t *testing.T) {
	d := shop(t)
	page := func(offset int) protocol.Query {
		return q(map[string]protocol.Node{
			"o": {Kind: "scan", Table: "orders", Scope: "o"},
			"chrono": {Kind: "order", Input: "o", Terms: []protocol.OrderTerm{
				{Expr: *protocol.Col("o", "placed_at")},
				{Expr: *protocol.Col("o", "id")},
			}},
			"page": {Kind: "slice", Input: "chrono", Offset: new(int(offset)), Limit: new(int(3))},
			"out": {Kind: "project", Input: "page",
				Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
		}, "out", "many")
	}
	d.Query(page(0)).Equals(`[{"id":"o1"},{"id":"o3"},{"id":"o4"}]`)
	d.Query(page(3)).Equals(`[{"id":"o2"},{"id":"o6"},{"id":"o5"}]`)
}
