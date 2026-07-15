package planner

// Nesting: crossings (first, array, scalar, exists) as projection fields,
// filter predicates, and order terms — the correlated shapes applications
// actually ask for. Every query is the literal wire tree; every expectation
// is hand-derived from the fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// 1. To-parent: each order with its customer as a nested object. The inner
// filter is an equality on the customers PK, so first is statically ≤1.
func TestNestToParentFirst(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"few": lirwire.Filter("o",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.LitOf("c1")),
				lirwire.Binary("lte", lirwire.Col("o", "placed_at"), lirwire.LitOf(2000)),
			})),
		"by_id": lirwire.Order("few",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"cu": lirwire.Scan("customers", "cu"),
		"owner": lirwire.Filter("cu",
			lirwire.Binary("eq", lirwire.Col("cu", "id"), lirwire.Col("o", "customer_id"))),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "customer", Expr: lirwire.First("owner")},
		}),
	}, "out", "many")).Equals(`[
		{"id":"o1","customer":{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null}},
		{"id":"o2","customer":{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null}}
	]`)
}

// 2. To-children: each customer with the ids of its orders; c5 has none.
func TestNestChildrenArray(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"c_by_id": lirwire.Order("c",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"mine_by_id": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"mine_ids": lirwire.Project("mine_by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("c_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "orders", Expr: lirwire.Array("mine_ids")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"c_by_id": lirwire.Order("c",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"newest": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"top2": lirwire.Slice("newest", 0, ptrInt(2)),
		"top2_ids": lirwire.Project("top2", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("c_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "recent", Expr: lirwire.Array("top2_ids")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"c_by_id": lirwire.Order("c",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"cnt": lirwire.Aggregate("mine", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"out": lirwire.Project("c_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "n", Expr: lirwire.Scalar("cnt")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"buyers": lirwire.Filter("c", lirwire.Exists("mine")),
		"by_id": lirwire.Order("buyers",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"}]`)
}

// 6. Not-exists in a filter: customers with no orders at all.
func TestNestNotExistsFilter(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"idle": lirwire.Filter("c", lirwire.Unary("not", lirwire.Exists("mine"))),
		"out": lirwire.Project("idle", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c5"}]`)
}

// 7. First over an explicitly ordered multi-row relation: the latest order
// per customer. The order node below the crossing is what makes it legal.
func TestNestFirstOverOrderedMultiRow(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"two": lirwire.Filter("c",
			lirwire.Binary("or",
				lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1")),
				lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c2")))),
		"two_by_id": lirwire.Order("two",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"newest": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"out": lirwire.Project("two_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "latest", Expr: lirwire.First("newest")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c1": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1"))),
		"u": lirwire.Scan("customers", "u"),
		"by_email": lirwire.Filter("u",
			lirwire.Binary("eq", lirwire.Col("u", "email"), lirwire.LitOf("bob@shop.io"))),
		"out": lirwire.Project("just_c1", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "bob", Expr: lirwire.First("by_email")},
		}),
	}, "out", "many")).Equals(`[
		{"id":"c1","bob":{"id":"c2","name":"Bob","email":"bob@shop.io","tier":"silver","created_at":200,"referrer_id":"c1"}}
	]`)
}

// 9. Two levels of nesting: customer → orders → items.
func TestNestTwoLevels(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c1": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1"))),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"mine_by_id": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"i": lirwire.Scan("order_items", "i"),
		"lines": lirwire.Filter("i",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"lines_by_id": lirwire.Order("lines",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("i", "id")}}),
		"line_out": lirwire.Project("lines_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("i", "id")},
			{As: "quantity", Expr: lirwire.Col("i", "quantity")},
		}),
		"o_shaped": lirwire.Project("mine_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "items", Expr: lirwire.Array("line_out")},
		}),
		"out": lirwire.Project("just_c1", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "orders", Expr: lirwire.Array("o_shaped")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c1": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1"))),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id")),
				lirwire.Binary("eq", lirwire.Col("o", "id"), lirwire.LitOf("o1")),
			})),
		"i": lirwire.Scan("order_items", "i"),
		"lines": lirwire.Filter("i",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"lines_by_id": lirwire.Order("lines",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("i", "id")}}),
		"p": lirwire.Scan("products", "p"),
		"prod": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "id"), lirwire.Col("i", "product_id"))),
		"prod_name": lirwire.Project("prod", "", nil,
			[]lirwire.Field{{As: "name", Expr: lirwire.Col("p", "name")}}),
		"line_shaped": lirwire.Project("lines_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("i", "id")},
			{As: "product", Expr: lirwire.First("prod_name")},
		}),
		"o_shaped": lirwire.Project("mine", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "items", Expr: lirwire.Array("line_shaped")},
		}),
		"out": lirwire.Project("just_c1", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "orders", Expr: lirwire.Array("o_shaped")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c2": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c2"))),
		"r": lirwire.Scan("customers", "r"),
		"ref": lirwire.Filter("r",
			lirwire.Binary("eq", lirwire.Col("r", "id"), lirwire.Col("c", "referrer_id"))),
		"oa": lirwire.Scan("orders", "oa"),
		"mine": lirwire.Filter("oa",
			lirwire.Binary("eq", lirwire.Col("oa", "customer_id"), lirwire.Col("c", "id"))),
		"mine_by_id": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("oa", "id")}}),
		"mine_ids": lirwire.Project("mine_by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("oa", "id")}}),
		"oc": lirwire.Scan("orders", "oc"),
		"mine_again": lirwire.Filter("oc",
			lirwire.Binary("eq", lirwire.Col("oc", "customer_id"), lirwire.Col("c", "id"))),
		"cnt": lirwire.Aggregate("mine_again", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"out": lirwire.Project("just_c2", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "referrer", Expr: lirwire.First("ref")},
			{As: "orders", Expr: lirwire.Array("mine_ids")},
			{As: "n", Expr: lirwire.Scalar("cnt")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"i": lirwire.Scan("order_items", "i"),
		"bulk_lines": lirwire.Filter("i",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id")),
				lirwire.Binary("gte", lirwire.Col("i", "quantity"), lirwire.LitOf(10)),
			})),
		"bulk": lirwire.Filter("o", lirwire.Exists("bulk_lines")),
		"by_id": lirwire.Order("bulk",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o6"}]`)
}

// 13. Anti-join via not-exists: products nobody has reviewed.
func TestNestAntiJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"r": lirwire.Scan("reviews", "r"),
		"revs": lirwire.Filter("r",
			lirwire.Binary("eq", lirwire.Col("r", "product_id"), lirwire.Col("p", "id"))),
		"unreviewed": lirwire.Filter("p",
			lirwire.Unary("not", lirwire.Exists("revs"))),
		"by_id": lirwire.Order("unreviewed",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

// 14. Scalar fold per parent including empty inputs: avg over no reviews is
// NULL, and the field is still present.
func TestNestScalarAvgIncludingEmpty(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"p_by_id": lirwire.Order("p",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"r": lirwire.Scan("reviews", "r"),
		"revs": lirwire.Filter("r",
			lirwire.Binary("eq", lirwire.Col("r", "product_id"), lirwire.Col("p", "id"))),
		"rating": lirwire.Aggregate("revs", "", nil,
			[]lirwire.AggTerm{{Fn: "avg", Arg: ptrExpr(lirwire.Col("r", "rating")), As: "avg_rating"}}),
		"out": lirwire.Project("p_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "avg_rating", Expr: lirwire.Scalar("rating")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"c_by_id": lirwire.Order("c",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"cnt": lirwire.Aggregate("mine", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"out": lirwire.Project("c_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "n", Expr: lirwire.Binary("add",
				lirwire.Scalar("cnt"), lirwire.LitOf(100))},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"r": lirwire.Scan("customers", "r"),
		"ref": lirwire.Filter("r",
			lirwire.Binary("eq", lirwire.Col("r", "id"), lirwire.Col("c", "referrer_id"))),
		"shaped": lirwire.Project("c", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "referrer", Expr: lirwire.First("ref")},
		}),
		"rootless": lirwire.Filter("shaped",
			lirwire.Unary("is_null", lirwire.Col("s", "referrer"))),
		"by_id": lirwire.Order("rootless",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("s", "id")}}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"cnt": lirwire.Aggregate("mine", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"busiest": lirwire.Order("c", []lirwire.OrderTerm{
			{Expr: lirwire.Scalar("cnt"), Desc: ptrBool(true)},
			{Expr: lirwire.Col("c", "id")},
		}),
		"out": lirwire.Project("busiest", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"},{"id":"c5"}]`)
}

// 18. Skip-level correlation: a scalar under the order-level projection whose
// sub-relation references the grandparent customer scope, not the order. Ada
// wrote 2 reviews (r1, r3), so every one of her orders reports 2.
func TestNestGrandparentCorrelation(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c1": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1"))),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"mine_by_id": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"rv": lirwire.Scan("reviews", "rv"),
		"my_reviews": lirwire.Filter("rv",
			lirwire.Binary("eq", lirwire.Col("rv", "customer_id"), lirwire.Col("c", "id"))),
		"rv_cnt": lirwire.Aggregate("my_reviews", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"o_shaped": lirwire.Project("mine_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "my_reviews", Expr: lirwire.Scalar("rv_cnt")},
		}),
		"out": lirwire.Project("just_c1", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "orders", Expr: lirwire.Array("o_shaped")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"p": lirwire.Scan("products", "p"),
		"dead_prod": lirwire.Filter("p",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("p", "id"), lirwire.Col("i", "product_id")),
				lirwire.Col("p", "discontinued"),
			})),
		"i": lirwire.Scan("order_items", "i"),
		"dead_lines": lirwire.Filter("i",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id")),
				lirwire.Exists("dead_prod"),
			})),
		"o": lirwire.Scan("orders", "o"),
		"dead_orders": lirwire.Filter("o",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id")),
				lirwire.Exists("dead_lines"),
			})),
		"hit": lirwire.Filter("c", lirwire.Exists("dead_orders")),
		"out": lirwire.Project("hit", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c3"}]`)
}

// 20. A grouped per-parent aggregate crossed as an array: order counts by
// status for each customer. The aggregate binds a scope so the order above
// it can address its outputs.
func TestNestGroupedAggArray(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"two": lirwire.Filter("c",
			lirwire.Binary("or",
				lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c1")),
				lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c2")))),
		"two_by_id": lirwire.Order("two",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"stats": lirwire.Aggregate("mine", "st",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "status")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"stats_ord": lirwire.Order("stats",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("st", "status")}}),
		"out": lirwire.Project("two_by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "by_status", Expr: lirwire.Array("stats_ord")},
		}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c5": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c5"))),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"mine_ids": lirwire.Project("mine", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("just_c5", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "orders", Expr: lirwire.Array("mine_ids")},
		}),
	}, "out", "many")).Equals(`[{"id":"c5","orders":[]}]`)
}

// 22. An absent first renders as a present key with null.
func TestNestAbsentFirstRendersNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"just_c5": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c5"))),
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"newest": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"out": lirwire.Project("just_c5", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "latest", Expr: lirwire.First("newest")},
		}),
	}, "out", "many")).Equals(`[{"id":"c5","latest":null}]`)
}
