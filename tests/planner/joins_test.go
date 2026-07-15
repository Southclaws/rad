package planner

// Joins: inner and left joins as literal wire trees — basic equi-joins,
// null padding, orphan detection, residual filters, join+aggregate, chains,
// self-joins, joins over derived inputs, non-equality conditions, and the
// binder's join rejections. Every expectation is hand-derived from the shop
// fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

func TestJoin_InnerBasic(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Every order with its customer's name. o1..o7 → Ada Ada Bob Bob Cyn Dee Ada.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"u": lirwire.Scan("customers", "u"),
		"j": lirwire.Join("o", "u", "inner",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("u", "id"))),
		"by_order": lirwire.Order("j",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_order", "", nil, []lirwire.Field{
			{As: "order", Expr: lirwire.Col("o", "id")},
			{As: "who", Expr: lirwire.Col("u", "name")},
		}),
	}, "out", "many")).Equals(`[
		{"order":"o1","who":"Ada"},
		{"order":"o2","who":"Ada"},
		{"order":"o3","who":"Bob"},
		{"order":"o4","who":"Bob"},
		{"order":"o5","who":"Cyn"},
		{"order":"o6","who":"Dee"},
		{"order":"o7","who":"Ada"}
	]`)
}

func TestJoin_LeftNullPadding(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Products with their review ratings; p4, p6, p7 have no reviews and get
	// a null-padded right side. Rating sorts ascending with NULLs first — the
	// documented order rule (lir.md: "NULLs first asc").
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"r": lirwire.Scan("reviews", "r"),
		"j": lirwire.Join("p", "r", "left",
			lirwire.Binary("eq", lirwire.Col("r", "product_id"), lirwire.Col("p", "id"))),
		"sorted": lirwire.Order("j", []lirwire.OrderTerm{
			{Expr: lirwire.Col("p", "id")},
			{Expr: lirwire.Col("r", "rating")},
		}),
		"out": lirwire.Project("sorted", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "rating", Expr: lirwire.Col("r", "rating")},
		}),
	}, "out", "many")).Equals(`[
		{"id":"p1","rating":4},
		{"id":"p1","rating":5},
		{"id":"p2","rating":2},
		{"id":"p3","rating":5},
		{"id":"p4","rating":null},
		{"id":"p5","rating":1},
		{"id":"p6","rating":null},
		{"id":"p7","rating":null}
	]`)
}

func TestJoin_LeftOrphansViaIsNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The classic anti-join spelling: left join, keep the null-padded rows.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"r": lirwire.Scan("reviews", "r"),
		"j": lirwire.Join("p", "r", "left",
			lirwire.Binary("eq", lirwire.Col("r", "product_id"), lirwire.Col("p", "id"))),
		"unreviewed": lirwire.Filter("j",
			lirwire.Unary("is_null", lirwire.Col("r", "id"))),
		"sorted": lirwire.Order("unreviewed",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"out": lirwire.Project("sorted", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

func TestJoin_ResidualFilterAbove(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Both join scopes stay visible above the join, so a filter can mix them.
	// Gold customers: c1, c3. Pending orders: o5 (c3), o7 (c1).
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"u": lirwire.Scan("customers", "u"),
		"j": lirwire.Join("o", "u", "inner",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("u", "id"))),
		"gold_pending": lirwire.Filter("j",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("u", "tier"), lirwire.LitOf("gold")),
				lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending")),
			})),
		"sorted": lirwire.Order("gold_pending",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("sorted", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "name", Expr: lirwire.Col("u", "name")},
		}),
	}, "out", "many")).Equals(`[
		{"id":"o5","name":"Cyn"},
		{"id":"o7","name":"Ada"}
	]`)
}

func TestJoin_AggregateRevenuePerCustomer(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Order totals: o1=130 o2=300 o3=115 o4=450 o5=500 o6=100 o7=120.
	// c1 = 130+300+120 = 550, c2 = 115+450 = 565, c3 = 500, c4 = 100.
	// c5 has no orders — absent under an inner join.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"i": lirwire.Scan("order_items", "i"),
		"j": lirwire.Join("o", "i", "inner",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"rev": lirwire.Aggregate("j", "rev",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "customer_id")}},
			[]lirwire.AggTerm{{
				Fn:  "sum",
				Arg: ptrExpr(lirwire.Binary("mul", lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))),
				As:  "revenue",
			}}),
		"sorted": lirwire.Order("rev",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("rev", "customer_id")}}),
	}, "sorted", "many")).Equals(`[
		{"customer_id":"c1","revenue":550},
		{"customer_id":"c2","revenue":565},
		{"customer_id":"c3","revenue":500},
		{"customer_id":"c4","revenue":100}
	]`)
}

func TestJoin_OrderByComputedThenSlice(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Top 3 line items by total. The unlabelled projection closes the scan
	// scopes, so the computed order sits below it, on the raw join output.
	// Totals: i7=500, i6=450, i3=300, then i8=100, i1/i4/i9=80, ...
	d.Query(q(map[string]lirwire.Node{
		"i": lirwire.Scan("order_items", "i"),
		"p": lirwire.Scan("products", "p"),
		"j": lirwire.Join("i", "p", "inner",
			lirwire.Binary("eq", lirwire.Col("i", "product_id"), lirwire.Col("p", "id"))),
		"by_total": lirwire.Order("j", []lirwire.OrderTerm{
			{Expr: lirwire.Binary("mul", lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price")), Desc: ptrBool(true)},
		}),
		"top3": lirwire.Slice("by_total", 0, ptrInt(3)),
		"out": lirwire.Project("top3", "", nil, []lirwire.Field{
			{As: "item", Expr: lirwire.Col("i", "id")},
			{As: "product", Expr: lirwire.Col("p", "name")},
			{As: "total", Expr: lirwire.Binary("mul", lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))},
		}),
	}, "out", "many")).Equals(`[
		{"item":"i7","product":"Chair","total":500},
		{"item":"i6","product":"Desk","total":450},
		{"item":"i3","product":"Monitor","total":300}
	]`)
}

func TestJoin_ThreeWayChain(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// items → orders → customers, two join nodes. c4's only order is o6,
	// whose only item is i8.
	d.Query(q(map[string]lirwire.Node{
		"i": lirwire.Scan("order_items", "i"),
		"o": lirwire.Scan("orders", "o"),
		"io": lirwire.Join("i", "o", "inner",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"c": lirwire.Scan("customers", "c"),
		"ioc": lirwire.Join("io", "c", "inner",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"dee": lirwire.Filter("ioc",
			lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.LitOf("c4"))),
		"out": lirwire.Project("dee", "", nil, []lirwire.Field{
			{As: "item", Expr: lirwire.Col("i", "id")},
			{As: "name", Expr: lirwire.Col("c", "name")},
		}),
	}, "out", "many")).Equals(`[{"item":"i8","name":"Dee"}]`)
}

func TestJoin_SelfLeftJoinReferrals(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Scope labels are query-wide unique, so the self-join binds "a" and "b".
	// c1 and c5 have NULL referrer_id: the ON is UNKNOWN, so they null-pad.
	d.Query(q(map[string]lirwire.Node{
		"a": lirwire.Scan("customers", "a"),
		"b": lirwire.Scan("customers", "b"),
		"j": lirwire.Join("a", "b", "left",
			lirwire.Binary("eq", lirwire.Col("a", "referrer_id"), lirwire.Col("b", "id"))),
		"sorted": lirwire.Order("j",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("a", "id")}}),
		"out": lirwire.Project("sorted", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("a", "id")},
			{As: "referrer", Expr: lirwire.Col("b", "name")},
		}),
	}, "out", "many")).Equals(`[
		{"id":"c1","referrer":null},
		{"id":"c2","referrer":"Ada"},
		{"id":"c3","referrer":"Ada"},
		{"id":"c4","referrer":"Bob"},
		{"id":"c5","referrer":null}
	]`)
}

func TestJoin_FilteredScansAsInputs(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Join inputs are ordinary relations: delivered orders (o1, o3, o6)
	// against gold customers (c1, c3). Only o1 belongs to a gold customer.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"delivered": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("delivered"))),
		"u": lirwire.Scan("customers", "u"),
		"gold": lirwire.Filter("u",
			lirwire.Binary("eq", lirwire.Col("u", "tier"), lirwire.LitOf("gold"))),
		"j": lirwire.Join("delivered", "gold", "inner",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("u", "id"))),
		"out": lirwire.Project("j", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o1"}]`)
}

func TestJoin_LabelledProjectionsAsInputs(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Labelled projections are joinable relations; their scopes ("lc", "ro")
	// address the join output. Pending orders: o5 → c3, o7 → c1.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"lp": lirwire.Project("c", "lc", nil, []lirwire.Field{
			{As: "cid", Expr: lirwire.Col("c", "id")},
			{As: "tier", Expr: lirwire.Col("c", "tier")},
		}),
		"o": lirwire.Scan("orders", "o"),
		"rp": lirwire.Project("o", "ro", nil, []lirwire.Field{
			{As: "ocust", Expr: lirwire.Col("o", "customer_id")},
			{As: "status", Expr: lirwire.Col("o", "status")},
		}),
		"j": lirwire.Join("lp", "rp", "inner",
			lirwire.Binary("eq", lirwire.Col("lc", "cid"), lirwire.Col("ro", "ocust"))),
		"pending": lirwire.Filter("j",
			lirwire.Binary("eq", lirwire.Col("ro", "status"), lirwire.LitOf("pending"))),
		"sorted": lirwire.Order("pending",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("lc", "cid")}}),
		"out": lirwire.Project("sorted", "", nil, []lirwire.Field{
			{As: "cid", Expr: lirwire.Col("lc", "cid")},
			{As: "status", Expr: lirwire.Col("ro", "status")},
		}),
	}, "out", "many")).Equals(`[
		{"cid":"c1","status":"pending"},
		{"cid":"c3","status":"pending"}
	]`)
}

func TestJoin_NonEqualityCondition(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// ON o.placed_at < c.created_at * 10 — a pure theta join. Orders placed
	// strictly before each customer's created_at*10:
	//   c1 (1000): none. c2 (2000): 1000,1500,1600 → 3.
	//   c3 (3000): +2000,2500 → 5. c4 (4000): +3000,3500 → 7. c5 (5000): 7.
	// c1 is absent — an inner join with no matches produces no group.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"j": lirwire.Join("c", "o", "inner",
			lirwire.Binary("lt", lirwire.Col("o", "placed_at"),
				lirwire.Binary("mul", lirwire.Col("c", "created_at"), lirwire.LitOf(10)))),
		"byc": lirwire.Aggregate("j", "byc",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("c", "id")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"sorted": lirwire.Order("byc",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("byc", "id")}}),
	}, "sorted", "many")).Equals(`[
		{"id":"c2","n":3},
		{"id":"c3","n":5},
		{"id":"c4","n":7},
		{"id":"c5","n":7}
	]`)
}

func TestJoin_SpreadBothSidesCollides(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// customers and orders both produce "id": spreading both scopes over a
	// join collides at bind time. Raw join rows need an explicit projection.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"j": lirwire.Join("c", "o", "inner",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"out": lirwire.Project("j", "", []string{"c", "o"}, nil),
	}, "out", "many")).ExpectStatus(422).ExpectError(`duplicate projection field "id"`)
}

func TestJoin_LeftJoinCountKeepsZeroCustomers(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// count(o.id) — count of the ARGUMENT — skips the null-padded right side,
	// so c5 (no orders) survives the left join with n=0, the SQL-standard
	// left-join-count idiom.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"j": lirwire.Join("c", "o", "left",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.Col("c", "id"))),
		"agg": lirwire.Aggregate("j", "agg",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("c", "id")}},
			[]lirwire.AggTerm{{Fn: "count", Arg: ptrExpr(lirwire.Col("o", "id")), As: "n"}}),
		"sorted": lirwire.Order("agg",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("agg", "id")}}),
	}, "sorted", "many")).Equals(`[
		{"id":"c1","n":3},
		{"id":"c2","n":2},
		{"id":"c3","n":1},
		{"id":"c4","n":1},
		{"id":"c5","n":0}
	]`)
}

func TestJoin_DuplicateScopeRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Scope labels are query-wide unique — two scans cannot share "x".
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "x"),
		"o": lirwire.Scan("orders", "x"),
		"j": lirwire.Join("c", "o", "inner",
			lirwire.Binary("eq", lirwire.Col("x", "customer_id"), lirwire.Col("x", "id"))),
	}, "j", "many")).ExpectError(`duplicate scope "x"`)
}

func TestJoin_CrossingInOnRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A join condition may not contain a sub-relation crossing — the binder
	// says to filter above the join instead.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Scan("orders", "o"),
		"r": lirwire.Scan("reviews", "r"),
		"j": lirwire.Join("c", "o", "inner",
			lirwire.Exists("r")),
	}, "j", "many")).ExpectError("a join condition cannot contain a sub-relation crossing")
}
