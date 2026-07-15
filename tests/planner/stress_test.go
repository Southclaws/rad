package planner

// Compositional-depth and adversarial cases: deep operator chains, crossings
// containing joins, aggregates of aggregates, and the preflight's structural
// rejections (cycles, sharing, dangling refs, unreachable nodes). Every
// expectation is hand-derived from the fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// 1. Three projections stacked, each renaming the previous one's output via
// its scope label. The filter sits below everything.
func TestStressTripleProjectionChain(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"gold": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold"))),
		"a": lirwire.Project("gold", "a", nil,
			[]lirwire.Field{{As: "n", Expr: lirwire.Col("c", "name")}}),
		"b": lirwire.Project("a", "b", nil,
			[]lirwire.Field{{As: "m", Expr: lirwire.Col("a", "n")}}),
		"out": lirwire.Project("b", "", nil,
			[]lirwire.Field{{As: "name", Expr: lirwire.Col("b", "m")}}),
	}, "out", "many")).Equals(`[{"name":"Ada"},{"name":"Cyn"}]`)
}

// 2. Order, project, order again — the LAST order wins. placed_at desc would
// give o7-first; the re-order by v.id asc restores o1..o7.
func TestStressOrderProjectOrderSandwich(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"newest": lirwire.Order("o",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"v": lirwire.Project("newest", "v", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "at", Expr: lirwire.Col("o", "placed_at")},
		}),
		"asc": lirwire.Order("v",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("v", "id")}}),
		"out": lirwire.Project("asc", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("v", "id")}}),
	}, "out", "many")).Equals(`[
		{"id":"o1"},{"id":"o2"},{"id":"o3"},{"id":"o4"},{"id":"o5"},{"id":"o6"},{"id":"o7"}
	]`)
}

// 3. Filter ABOVE a slice: take the 3 newest orders (o7,o5,o6), THEN keep
// delivered. Only o6 survives — slice-then-filter is not filter-then-slice.
func TestStressFilterAboveSlice(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"newest": lirwire.Order("o",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"top3": lirwire.Slice("newest", 0, ptrInt(3)),
		"delivered": lirwire.Filter("top3",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("delivered"))),
		"out": lirwire.Project("delivered", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o6"}]`)
}

// 4. Aggregate above a slice: fold over exactly the top-3 page (o7,o5,o6),
// not the whole table.
func TestStressAggregateAboveSlice(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"newest": lirwire.Order("o",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"top3": lirwire.Slice("newest", 0, ptrInt(3)),
		"fold": lirwire.Aggregate("top3", "", nil, []lirwire.AggTerm{
			{Fn: "count", As: "n"},
			{Fn: "max", Arg: ptrExpr(lirwire.Col("o", "placed_at")), As: "max"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"n":3,"max":3500}]`)
}

// 5. A scalar crossing whose sub-relation contains a JOIN: per gold customer,
// total spend = sum(qty*unit_price) over their orders joined to items.
// c1: o1(130)+o2(300)+o7(120)=550; c3: o5(500)=500.
func TestStressCrossingContainingJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"gold": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold"))),
		"o": lirwire.Scan("orders", "o"),
		"i": lirwire.Scan("order_items", "i"),
		"lines": lirwire.Join("o", "i", "inner",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"mine": lirwire.Filter("lines",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"spend": lirwire.Aggregate("mine", "", nil, []lirwire.AggTerm{
			{Fn: "sum", Arg: ptrExpr(lirwire.Binary("mul",
				lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))), As: "spent"},
		}),
		"out": lirwire.Project("gold", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "spent", Expr: lirwire.Scalar("spend")},
		}),
	}, "out", "many")).Equals(`[{"id":"c1","spent":550},{"id":"c3","spent":500}]`)
}

// 6. Join whose LEFT side is a labelled projection that itself carries a
// scalar crossing (per-customer order count), joined against a plain orders
// scan, filtered to pending above the join.
func TestStressJoinOfProjectionsWithCrossing(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":  lirwire.Scan("customers", "c"),
		"co": lirwire.Scan("orders", "co"),
		"mine": lirwire.Filter("co",
			lirwire.Binary("eq", lirwire.Col("co", "customer_id"), lirwire.Col("c", "id"))),
		"n": lirwire.Aggregate("mine", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"cc": lirwire.Project("c", "cc", nil, []lirwire.Field{
			{As: "cid", Expr: lirwire.Col("c", "id")},
			{As: "n_orders", Expr: lirwire.Scalar("n")},
		}),
		"o": lirwire.Scan("orders", "o"),
		"j": lirwire.Join("cc", "o", "inner",
			lirwire.Binary("eq", lirwire.Col("cc", "cid"), lirwire.Col("o", "customer_id"))),
		"pending": lirwire.Filter("j",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))),
		"by_order": lirwire.Order("pending",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_order", "", nil, []lirwire.Field{
			{As: "cid", Expr: lirwire.Col("cc", "cid")},
			{As: "n", Expr: lirwire.Col("cc", "n_orders")},
			{As: "order", Expr: lirwire.Col("o", "id")},
		}),
	}, "out", "many")).Equals(`[
		{"cid":"c3","n":1,"order":"o5"},
		{"cid":"c1","n":3,"order":"o7"}
	]`)
}

// 7. Six operators in one straight chain. placed_at >= 1500 ascending is
// o3,o4,o2,o6,o5,o7; offset 1 limit 4 keeps o4,o2,o6,o5; the filter above
// the labelled projection drops cancelled o4.
func TestStressSixDeepChain(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"recent": lirwire.Filter("o",
			lirwire.Binary("gte", lirwire.Col("o", "placed_at"), lirwire.LitOf(1500))),
		"chrono": lirwire.Order("recent",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at")}}),
		"page": lirwire.Slice("chrono", 1, ptrInt(4)),
		"w": lirwire.Project("page", "w", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "status", Expr: lirwire.Col("o", "status")},
		}),
		"live": lirwire.Filter("w",
			lirwire.Binary("ne", lirwire.Col("w", "status"), lirwire.LitOf("cancelled"))),
		"out": lirwire.Project("live", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("w", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o2"},{"id":"o6"},{"id":"o5"}]`)
}

// 8. Four levels of nesting: customer → orders array → items array → product
// object, projecting only the product's name at the deepest level. Item ids
// are TEXT, so "by id" is lexicographic: o7's items order i10 before i9.
func TestStressFourLevelNesting(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"c1": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1"))),

		"p": lirwire.Scan("products", "p"),
		"prod": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "id"), lirwire.Col("i", "product_id"))),
		"pname": lirwire.Project("prod", "", nil,
			[]lirwire.Field{{As: "name", Expr: lirwire.Col("p", "name")}}),

		"i": lirwire.Scan("order_items", "i"),
		"oi": lirwire.Filter("i",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"oi_by_id": lirwire.Order("oi",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("i", "id")}}),
		"item_out": lirwire.Project("oi_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("i", "id")},
			{As: "product", Expr: lirwire.First("pname")},
		}),

		"o": lirwire.Scan("orders", "o"),
		"my_orders": lirwire.Filter("o",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id")),
				lirwire.Binary("ne", lirwire.Col("o", "status"), lirwire.LitOf("cancelled")),
			})),
		"chrono": lirwire.Order("my_orders",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at")}}),
		"order_out": lirwire.Project("chrono", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "items", Expr: lirwire.Array("item_out")},
		}),

		"out": lirwire.Project("c1", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "orders", Expr: lirwire.Array("order_out")},
		}),
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
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"i": lirwire.Scan("order_items", "i"),
		"lines": lirwire.Join("o", "i", "inner",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"mine": lirwire.Filter("lines",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"spend": lirwire.Aggregate("mine", "", nil, []lirwire.AggTerm{
			{Fn: "sum", Arg: ptrExpr(lirwire.Binary("mul",
				lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))), As: "spend"},
		}),
		"s": lirwire.Project("c", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "spend", Expr: lirwire.Scalar("spend")},
		}),
		"ranked": lirwire.Order("s",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("s", "spend"), Desc: ptrBool(true)}}),
		"top2": lirwire.Slice("ranked", 0, ptrInt(2)),
	}, "top2", "many")).Equals(`[
		{"id":"c2","spend":565},
		{"id":"c1","spend":550}
	]`)
}

// 10. Aggregate of an aggregate: per-customer order counts, then the average
// count. FOOTGUN: c5 has no orders, so it never forms a group — the average
// is over the 4 customers who DO have orders: (3+2+1+1)/4 = 1.75, not 7/5.
func TestStressAggregateOfAggregate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"per": lirwire.Aggregate("o", "per",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "customer_id")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"overall": lirwire.Aggregate("per", "", nil, []lirwire.AggTerm{
			{Fn: "avg", Arg: ptrExpr(lirwire.Col("per", "n")), As: "avg_n"},
		}),
	}, "overall", "exactly_one")).Equals(`[{"avg_n":1.75}]`)
}

// 11. HAVING via a wrapped crossing: keep customers whose delivered-order
// count >= 1. Delivered: c1 (o1), c2 (o3), c4 (o6); c3 and c5 have zero
// (count is never NULL, so gte(0,1) is plain FALSE, not UNKNOWN).
func TestStressWrappedCrossingHaving(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"delivered": lirwire.Filter("o",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id")),
				lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("delivered")),
			})),
		"n": lirwire.Aggregate("delivered", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"keep": lirwire.Filter("c",
			lirwire.Binary("gte", lirwire.Scalar("n"), lirwire.LitOf(1))),
		"out": lirwire.Project("keep", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c4"}]`)
}

// 12. Arithmetic over a column that is itself computed by a labelled
// projection below: total = qty*unit_price, then taxed = total*2.
// o1's items: i1 total 80 → 160, i2 total 50 → 100.
func TestStressArithmeticOverComputedColumn(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"i": lirwire.Scan("order_items", "i"),
		"o1_items": lirwire.Filter("i",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.LitOf("o1"))),
		"l": lirwire.Project("o1_items", "l", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("i", "id")},
			{As: "total", Expr: lirwire.Binary("mul", lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))},
		}),
		"by_id": lirwire.Order("l",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("l", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("l", "id")},
			{As: "taxed", Expr: lirwire.Binary("mul", lirwire.Col("l", "total"), lirwire.LitOf(2))},
		}),
	}, "out", "many")).Equals(`[{"id":"i1","taxed":160},{"id":"i2","taxed":100}]`)
}

// 13. Filter above a labelled projection referencing a RENAMED column: the
// original names (email, tier) are gone above "v"; only v.handle and v.t
// exist there.
func TestStressFilterAboveRenamedProjection(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"v": lirwire.Project("c", "v", nil, []lirwire.Field{
			{As: "handle", Expr: lirwire.Col("c", "email")},
			{As: "t", Expr: lirwire.Col("c", "tier")},
		}),
		"gold": lirwire.Filter("v",
			lirwire.Binary("eq", lirwire.Col("v", "t"), lirwire.LitOf("gold"))),
		"out": lirwire.Project("gold", "", nil,
			[]lirwire.Field{{As: "handle", Expr: lirwire.Col("v", "handle")}}),
	}, "out", "many")).Equals(`[{"handle":"ada@shop.io"},{"handle":"cyn@shop.io"}]`)
}

// 14. An orphan node nothing references is a dead definition, rejected during
// lowering (the one wire-graph property the binder can't see, since lowering
// discards unreachable nodes before it runs).
func TestStressUnreachableNodeRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"out": lirwire.Project("c", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
		"orphan": lirwire.Scan("orders", "z"),
	}, "out", "many")).ExpectStatus(422).ExpectError("unreachable node definitions")
}

// 15. A reference to a node id that does not exist is rejected.
func TestStressDanglingReferenceRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(lirwire.Query{Nodes: map[string]lirwire.Node{
		"bad": lirwire.Filter("nope",
			lirwire.Binary("eq", lirwire.LitOf(1), lirwire.LitOf(1))),
	}, Root: lirwire.Root{Node: "bad", Cardinality: "many"}}).ExpectError(`unknown node "nope"`)
}

// 16. Single-consumer law: one sub-node consumed by TWO crossings in the same
// projection is sharing, and LIR has no sharing semantics.
func TestStressSharedNodeRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"out": lirwire.Project("c", "", nil, []lirwire.Field{
			{As: "has_any", Expr: lirwire.Exists("mine")},
			{As: "has_any_again", Expr: lirwire.Exists("mine")},
		}),
	}, "out", "many")).ExpectError("duplicate scope")
}

// 17. Two filters consuming each other form a wire cycle; value nodes cannot
// mean anything cyclic, so the graph decoder rejects it outright.
func TestStressCycleRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(lirwire.Query{Nodes: map[string]lirwire.Node{
		"a": lirwire.Filter("b",
			lirwire.Binary("eq", lirwire.LitOf(1), lirwire.LitOf(1))),
		"b": lirwire.Filter("a",
			lirwire.Binary("eq", lirwire.LitOf(1), lirwire.LitOf(1))),
	}, Root: lirwire.Root{Node: "a", Cardinality: "many"}}).ExpectError("part of a cycle")
}

// 18. Root cardinality "first" over an ordered relation: exactly one record,
// which the wire (and thus the harness) sees as a single-element record list.
func TestStressRootFirst(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"newest": lirwire.Order("o",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
	}, "newest", "first")).Equals(`[
		{"id":"o7","customer_id":"c1","status":"pending","placed_at":3500,"discount":null}
	]`)
}

// 19. exactly_one over a multi-row relation is a runtime cardinality
// violation. It surfaces as an "exec:" engine error, which the server relays
// as a 422 client problem — not a 500.
func TestStressExactlyOneViolation(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
	}, "o", "exactly_one")).ExpectStatus(422).ExpectCode("execution_failed").ExpectError("exactly one")
}

// 20. Pagination disjointness: the same total order (placed_at asc, id
// tiebreak) sliced at [0,3) and [3,6) yields disjoint, exactly-adjacent
// pages. Ascending placed_at: o1,o3,o4,o2,o6,o5,o7.
func TestStressPaginationDisjoint(t *testing.T) {
	t.Parallel()
	d := shop(t)
	page := func(offset int) lirwire.Query {
		return q(map[string]lirwire.Node{
			"o": lirwire.Scan("orders", "o"),
			"chrono": lirwire.Order("o", []lirwire.OrderTerm{
				{Expr: lirwire.Col("o", "placed_at")},
				{Expr: lirwire.Col("o", "id")},
			}),
			"page": lirwire.Slice("chrono", offset, ptrInt(3)),
			"out": lirwire.Project("page", "", nil,
				[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
		}, "out", "many")
	}
	d.Query(page(0)).Equals(`[{"id":"o1"},{"id":"o3"},{"id":"o4"}]`)
	d.Query(page(3)).Equals(`[{"id":"o2"},{"id":"o6"},{"id":"o5"}]`)
}
