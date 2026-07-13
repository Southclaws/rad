package planner

// Nesting: crossings (first, array, scalar, exists) as projection fields,
// filter predicates, and order terms — the correlated shapes applications
// actually ask for. Every query is the literal wire tree; every expectation
// is hand-derived from the fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

// 1. To-parent: each order with its customer as a nested object. The inner
// filter is an equality on the customers PK, so first is statically ≤1.
func TestNestToParentFirst(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"few": {Kind: "filter", Input: "o",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("o", "customer_id"), protocol.Lit("c1")),
				protocol.Lte(protocol.Col("o", "placed_at"), protocol.Lit(2000)),
			})},
		"by_id": {Kind: "order", Input: "few",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"cu": {Kind: "scan", Table: "customers", Scope: "cu"},
		"owner": {Kind: "filter", Input: "cu",
			Predicate: protocol.Eq(protocol.Col("cu", "id"), protocol.Col("o", "customer_id"))},
		"out": {Kind: "project", Input: "by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "customer", Expr: *protocol.FirstOf("owner")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"o1","customer":{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null}},
		{"id":"o2","customer":{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null}}
	]`)
}

// 2. To-children: each customer with the ids of its orders; c5 has none.
func TestNestChildrenArray(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"c_by_id": {Kind: "order", Input: "c",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"mine_by_id": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"mine_ids": {Kind: "project", Input: "mine_by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "c_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "orders", Expr: *protocol.ArrayOf("mine_ids")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","orders":[{"id":"o1"},{"id":"o2"},{"id":"o7"}]},
		{"id":"c2","orders":[{"id":"o3"},{"id":"o4"}]},
		{"id":"c3","orders":[{"id":"o5"}]},
		{"id":"c4","orders":[{"id":"o6"}]},
		{"id":"c5","orders":[]}
	]`)
}

// 3. Top-N per parent: each customer's 2 most recent orders — a per-key
// slice under an array crossing.
func TestNestTopNPerParent(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"c_by_id": {Kind: "order", Input: "c",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"newest": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"top2": {Kind: "slice", Input: "newest", Limit: new(int(2))},
		"top2_ids": {Kind: "project", Input: "top2",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "c_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "recent", Expr: *protocol.ArrayOf("top2_ids")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","recent":[{"id":"o7"},{"id":"o2"}]},
		{"id":"c2","recent":[{"id":"o4"},{"id":"o3"}]},
		{"id":"c3","recent":[{"id":"o5"}]},
		{"id":"c4","recent":[{"id":"o6"}]},
		{"id":"c5","recent":[]}
	]`)
}

// 4. Scalar count per parent: an ungrouped fold is exactly-one-row and
// one-column, so it crosses as a scalar; count over no rows is 0.
func TestNestScalarCountPerParent(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"c_by_id": {Kind: "order", Input: "c",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"cnt": {Kind: "aggregate", Input: "mine",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"out": {Kind: "project", Input: "c_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "n", Expr: *protocol.ScalarOf("cnt")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","n":3},
		{"id":"c2","n":2},
		{"id":"c3","n":1},
		{"id":"c4","n":1},
		{"id":"c5","n":0}
	]`)
}

// 5. Exists in a filter: customers who have placed any order.
func TestNestExistsFilter(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"buyers": {Kind: "filter", Input: "c", Predicate: protocol.Exists("mine")},
		"by_id": {Kind: "order", Input: "buyers",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"}]`)
}

// 6. Not-exists in a filter: customers with no orders at all.
func TestNestNotExistsFilter(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"idle": {Kind: "filter", Input: "c", Predicate: protocol.Not(protocol.Exists("mine"))},
		"out": {Kind: "project", Input: "idle",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c5"}]`)
}

// 7. First over an explicitly ordered multi-row relation: the latest order
// per customer. The order node below the crossing is what makes it legal.
func TestNestFirstOverOrderedMultiRow(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"two": {Kind: "filter", Input: "c",
			Predicate: protocol.Or(
				protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1")),
				protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c2")))},
		"two_by_id": {Kind: "order", Input: "two",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"newest": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"out": {Kind: "project", Input: "two_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "latest", Expr: *protocol.FirstOf("newest")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","latest":{"id":"o7","customer_id":"c1","status":"pending","placed_at":3500,"discount":null}},
		{"id":"c2","latest":{"id":"o4","customer_id":"c2","status":"cancelled","placed_at":1600,"discount":null}}
	]`)
}

// 8. First legal without any order: an equality on a unique index (email)
// is statically at-most-one.
func TestNestFirstViaUniqueIndex(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1"))},
		"u": {Kind: "scan", Table: "customers", Scope: "u"},
		"by_email": {Kind: "filter", Input: "u",
			Predicate: protocol.Eq(protocol.Col("u", "email"), protocol.Lit("bob@shop.io"))},
		"out": {Kind: "project", Input: "just_c1", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "bob", Expr: *protocol.FirstOf("by_email")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","bob":{"id":"c2","name":"Bob","email":"bob@shop.io","tier":"silver","created_at":200,"referrer_id":"c1"}}
	]`)
}

// 9. Two levels of nesting: customer → orders → items.
func TestNestTwoLevels(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1"))},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"mine_by_id": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"lines": {Kind: "filter", Input: "i",
			Predicate: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"lines_by_id": {Kind: "order", Input: "lines",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("i", "id")}}},
		"line_out": {Kind: "project", Input: "lines_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("i", "id")},
			{As: "quantity", Expr: *protocol.Col("i", "quantity")},
		}},
		"o_shaped": {Kind: "project", Input: "mine_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "items", Expr: *protocol.ArrayOf("line_out")},
		}},
		"out": {Kind: "project", Input: "just_c1", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "orders", Expr: *protocol.ArrayOf("o_shaped")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","orders":[
			{"id":"o1","items":[{"id":"i1","quantity":1},{"id":"i2","quantity":10}]},
			{"id":"o2","items":[{"id":"i3","quantity":1}]},
			{"id":"o7","items":[{"id":"i10","quantity":1},{"id":"i9","quantity":1}]}
		]}
	]`)
}

// 10. Three levels: customer → order → items → each item's product (a
// to-parent first at the deepest level, projected down to the name).
func TestNestThreeLevelsWithParent(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1"))},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id")),
				protocol.Eq(protocol.Col("o", "id"), protocol.Lit("o1")),
			})},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"lines": {Kind: "filter", Input: "i",
			Predicate: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"lines_by_id": {Kind: "order", Input: "lines",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("i", "id")}}},
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"prod": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "id"), protocol.Col("i", "product_id"))},
		"prod_name": {Kind: "project", Input: "prod",
			Fields: []protocol.Field{{As: "name", Expr: *protocol.Col("p", "name")}}},
		"line_shaped": {Kind: "project", Input: "lines_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("i", "id")},
			{As: "product", Expr: *protocol.FirstOf("prod_name")},
		}},
		"o_shaped": {Kind: "project", Input: "mine", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "items", Expr: *protocol.ArrayOf("line_shaped")},
		}},
		"out": {Kind: "project", Input: "just_c1", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "orders", Expr: *protocol.ArrayOf("o_shaped")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","orders":[
			{"id":"o1","items":[
				{"id":"i1","product":{"name":"Keyboard"}},
				{"id":"i2","product":{"name":"Notebook"}}
			]}
		]}
	]`)
}

// 11. Three sibling crossings on one projection: referrer (first), order ids
// (array), order count (scalar) — each with its own sub-tree and scope.
func TestNestSiblingCrossings(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c2": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c2"))},
		"r": {Kind: "scan", Table: "customers", Scope: "r"},
		"ref": {Kind: "filter", Input: "r",
			Predicate: protocol.Eq(protocol.Col("r", "id"), protocol.Col("c", "referrer_id"))},
		"oa": {Kind: "scan", Table: "orders", Scope: "oa"},
		"mine": {Kind: "filter", Input: "oa",
			Predicate: protocol.Eq(protocol.Col("oa", "customer_id"), protocol.Col("c", "id"))},
		"mine_by_id": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("oa", "id")}}},
		"mine_ids": {Kind: "project", Input: "mine_by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("oa", "id")}}},
		"oc": {Kind: "scan", Table: "orders", Scope: "oc"},
		"mine_again": {Kind: "filter", Input: "oc",
			Predicate: protocol.Eq(protocol.Col("oc", "customer_id"), protocol.Col("c", "id"))},
		"cnt": {Kind: "aggregate", Input: "mine_again",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"out": {Kind: "project", Input: "just_c2", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "referrer", Expr: *protocol.FirstOf("ref")},
			{As: "orders", Expr: *protocol.ArrayOf("mine_ids")},
			{As: "n", Expr: *protocol.ScalarOf("cnt")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c2",
		 "referrer":{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null},
		 "orders":[{"id":"o3"},{"id":"o4"}],
		 "n":2}
	]`)
}

// 12. A crossing with a residual predicate inside a filter: orders that
// contain any line of 10 or more units.
func TestNestCrossingInFilter(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"bulk_lines": {Kind: "filter", Input: "i",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id")),
				protocol.Gte(protocol.Col("i", "quantity"), protocol.Lit(10)),
			})},
		"bulk": {Kind: "filter", Input: "o", Predicate: protocol.Exists("bulk_lines")},
		"by_id": {Kind: "order", Input: "bulk",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o6"}]`)
}

// 13. Anti-join via not-exists: products nobody has reviewed.
func TestNestAntiJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"r": {Kind: "scan", Table: "reviews", Scope: "r"},
		"revs": {Kind: "filter", Input: "r",
			Predicate: protocol.Eq(protocol.Col("r", "product_id"), protocol.Col("p", "id"))},
		"unreviewed": {Kind: "filter", Input: "p",
			Predicate: protocol.Not(protocol.Exists("revs"))},
		"by_id": {Kind: "order", Input: "unreviewed",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("p", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

// 14. Scalar fold per parent including empty inputs: avg over no reviews is
// NULL, and the field is still present.
func TestNestScalarAvgIncludingEmpty(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"p_by_id": {Kind: "order", Input: "p",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("p", "id")}}},
		"r": {Kind: "scan", Table: "reviews", Scope: "r"},
		"revs": {Kind: "filter", Input: "r",
			Predicate: protocol.Eq(protocol.Col("r", "product_id"), protocol.Col("p", "id"))},
		"rating": {Kind: "aggregate", Input: "revs",
			Aggs: []protocol.AggTerm{{Fn: "avg", Arg: protocol.Col("r", "rating"), As: "avg_rating"}}},
		"out": {Kind: "project", Input: "p_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("p", "id")},
			{As: "avg_rating", Expr: *protocol.ScalarOf("rating")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"p1","avg_rating":4.5},
		{"id":"p2","avg_rating":2},
		{"id":"p3","avg_rating":5},
		{"id":"p4","avg_rating":null},
		{"id":"p5","avg_rating":1},
		{"id":"p6","avg_rating":null},
		{"id":"p7","avg_rating":null}
	]`)
}

// 15. A scalar crossing wrapped in arithmetic: order count + 100. Wrapping a
// crossing must not change how it executes.
func TestNestScalarInArithmetic(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"c_by_id": {Kind: "order", Input: "c",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"cnt": {Kind: "aggregate", Input: "mine",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"out": {Kind: "project", Input: "c_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "n", Expr: protocol.Expr{Kind: "binary", Op: "add",
				Left: protocol.ScalarOf("cnt"), Right: protocol.Lit(100)}},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","n":103},
		{"id":"c2","n":102},
		{"id":"c3","n":101},
		{"id":"c4","n":101},
		{"id":"c5","n":100}
	]`)
}

// 16. is_null over a first crossing, referenced through a labelled
// projection's scope: customers whose referrer lookup came back empty.
func TestNestIsNullFirstInFilter(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"r": {Kind: "scan", Table: "customers", Scope: "r"},
		"ref": {Kind: "filter", Input: "r",
			Predicate: protocol.Eq(protocol.Col("r", "id"), protocol.Col("c", "referrer_id"))},
		"shaped": {Kind: "project", Input: "c", Scope: "s", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "referrer", Expr: *protocol.FirstOf("ref")},
		}},
		"rootless": {Kind: "filter", Input: "shaped",
			Predicate: protocol.IsNull(protocol.Col("s", "referrer"))},
		"by_id": {Kind: "order", Input: "rootless",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("s", "id")}}},
	}, "by_id", "many")).Equals(`[
		{"id":"c1","referrer":null},
		{"id":"c5","referrer":null}
	]`)
}

// 17. A crossing as an order term: customers by descending order count, ties
// broken by id — c3/c4 both have one order.
func TestNestCrossingAsOrderTerm(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"cnt": {Kind: "aggregate", Input: "mine",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"busiest": {Kind: "order", Input: "c", Terms: []protocol.OrderTerm{
			{Expr: *protocol.ScalarOf("cnt"), Desc: true},
			{Expr: *protocol.Col("c", "id")},
		}},
		"out": {Kind: "project", Input: "busiest",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"},{"id":"c5"}]`)
}

// 18. Skip-level correlation: a scalar under the order-level projection whose
// sub-relation references the grandparent customer scope, not the order. Ada
// wrote 2 reviews (r1, r3), so every one of her orders reports 2.
func TestNestGrandparentCorrelation(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1"))},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"mine_by_id": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"rv": {Kind: "scan", Table: "reviews", Scope: "rv"},
		"my_reviews": {Kind: "filter", Input: "rv",
			Predicate: protocol.Eq(protocol.Col("rv", "customer_id"), protocol.Col("c", "id"))},
		"rv_cnt": {Kind: "aggregate", Input: "my_reviews",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"o_shaped": {Kind: "project", Input: "mine_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "my_reviews", Expr: *protocol.ScalarOf("rv_cnt")},
		}},
		"out": {Kind: "project", Input: "just_c1", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "orders", Expr: *protocol.ArrayOf("o_shaped")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","orders":[
			{"id":"o1","my_reviews":2},
			{"id":"o2","my_reviews":2},
			{"id":"o7","my_reviews":2}
		]}
	]`)
}

// 19. Exists nested inside exists inside exists: customers who ever ordered
// a discontinued product. Only the Chair (p5) is discontinued; it appears on
// i7 → o5 → c3.
func TestNestDoubleNestedExists(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"dead_prod": {Kind: "filter", Input: "p",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("p", "id"), protocol.Col("i", "product_id")),
				protocol.Col("p", "discontinued"),
			})},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"dead_lines": {Kind: "filter", Input: "i",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id")),
				protocol.Exists("dead_prod"),
			})},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"dead_orders": {Kind: "filter", Input: "o",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id")),
				protocol.Exists("dead_lines"),
			})},
		"hit": {Kind: "filter", Input: "c", Predicate: protocol.Exists("dead_orders")},
		"out": {Kind: "project", Input: "hit",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c3"}]`)
}

// 20. A grouped per-parent aggregate crossed as an array: order counts by
// status for each customer. The aggregate binds a scope so the order above
// it can address its outputs.
func TestNestGroupedAggArray(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"two": {Kind: "filter", Input: "c",
			Predicate: protocol.Or(
				protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c1")),
				protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c2")))},
		"two_by_id": {Kind: "order", Input: "two",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"stats": {Kind: "aggregate", Input: "mine", Scope: "st",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "status")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"stats_ord": {Kind: "order", Input: "stats",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("st", "status")}}},
		"out": {Kind: "project", Input: "two_by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "by_status", Expr: *protocol.ArrayOf("stats_ord")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","by_status":[
			{"status":"delivered","n":1},
			{"status":"pending","n":1},
			{"status":"shipped","n":1}
		]},
		{"id":"c2","by_status":[
			{"status":"cancelled","n":1},
			{"status":"delivered","n":1}
		]}
	]`)
}

// 21. An empty child set renders as [], never null.
func TestNestEmptyArrayRendersEmpty(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c5": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c5"))},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"mine_ids": {Kind: "project", Input: "mine",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "just_c5", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "orders", Expr: *protocol.ArrayOf("mine_ids")},
		}},
	}, "out", "many")).Equals(`[{"id":"c5","orders":[]}]`)
}

// 22. An absent first renders as a present key with null.
func TestNestAbsentFirstRendersNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"just_c5": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c5"))},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"newest": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"out": {Kind: "project", Input: "just_c5", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "latest", Expr: *protocol.FirstOf("newest")},
		}},
	}, "out", "many")).Equals(`[{"id":"c5","latest":null}]`)
}
