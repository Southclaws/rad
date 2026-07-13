package planner

// Joins: inner and left joins as literal wire trees — basic equi-joins,
// null padding, orphan detection, residual filters, join+aggregate, chains,
// self-joins, joins over derived inputs, non-equality conditions, and the
// binder's join rejections. Every expectation is hand-derived from the shop
// fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

func TestJoin_InnerBasic(t *testing.T) {
	d := shop(t)
	// Every order with its customer's name. o1..o7 → Ada Ada Bob Bob Cyn Dee Ada.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"u": {Kind: "scan", Table: "customers", Scope: "u"},
		"j": {Kind: "join", Left: "o", Right: "u", Join: "inner",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("u", "id"))},
		"by_order": {Kind: "order", Input: "j",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_order", Fields: []protocol.Field{
			{As: "order", Expr: *protocol.Col("o", "id")},
			{As: "who", Expr: *protocol.Col("u", "name")},
		}},
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
	d := shop(t)
	// Products with their review ratings; p4, p6, p7 have no reviews and get
	// a null-padded right side. Rating sorts ascending with NULLs first — the
	// documented order rule (lir.md: "NULLs first asc").
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"r": {Kind: "scan", Table: "reviews", Scope: "r"},
		"j": {Kind: "join", Left: "p", Right: "r", Join: "left",
			On: protocol.Eq(protocol.Col("r", "product_id"), protocol.Col("p", "id"))},
		"sorted": {Kind: "order", Input: "j", Terms: []protocol.OrderTerm{
			{Expr: *protocol.Col("p", "id")},
			{Expr: *protocol.Col("r", "rating")},
		}},
		"out": {Kind: "project", Input: "sorted", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("p", "id")},
			{As: "rating", Expr: *protocol.Col("r", "rating")},
		}},
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
	d := shop(t)
	// The classic anti-join spelling: left join, keep the null-padded rows.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"r": {Kind: "scan", Table: "reviews", Scope: "r"},
		"j": {Kind: "join", Left: "p", Right: "r", Join: "left",
			On: protocol.Eq(protocol.Col("r", "product_id"), protocol.Col("p", "id"))},
		"unreviewed": {Kind: "filter", Input: "j",
			Predicate: protocol.IsNull(protocol.Col("r", "id"))},
		"sorted": {Kind: "order", Input: "unreviewed",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("p", "id")}}},
		"out": {Kind: "project", Input: "sorted",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

func TestJoin_ResidualFilterAbove(t *testing.T) {
	d := shop(t)
	// Both join scopes stay visible above the join, so a filter can mix them.
	// Gold customers: c1, c3. Pending orders: o5 (c3), o7 (c1).
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"u": {Kind: "scan", Table: "customers", Scope: "u"},
		"j": {Kind: "join", Left: "o", Right: "u", Join: "inner",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("u", "id"))},
		"gold_pending": {Kind: "filter", Input: "j",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("u", "tier"), protocol.Lit("gold")),
				protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending")),
			})},
		"sorted": {Kind: "order", Input: "gold_pending",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "sorted", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "name", Expr: *protocol.Col("u", "name")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"o5","name":"Cyn"},
		{"id":"o7","name":"Ada"}
	]`)
}

func TestJoin_AggregateRevenuePerCustomer(t *testing.T) {
	d := shop(t)
	// Order totals: o1=130 o2=300 o3=115 o4=450 o5=500 o6=100 o7=120.
	// c1 = 130+300+120 = 550, c2 = 115+450 = 565, c3 = 500, c4 = 100.
	// c5 has no orders — absent under an inner join.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"j": {Kind: "join", Left: "o", Right: "i", Join: "inner",
			On: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"rev": {Kind: "aggregate", Input: "j", Scope: "rev",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "customer_id")}},
			Aggs: []protocol.AggTerm{{Fn: "sum",
				Arg: &protocol.Expr{Kind: "binary", Op: "mul",
					Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")},
				As: "revenue"}}},
		"sorted": {Kind: "order", Input: "rev",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("rev", "customer_id")}}},
	}, "sorted", "many")).Equals(`[
		{"customer_id":"c1","revenue":550},
		{"customer_id":"c2","revenue":565},
		{"customer_id":"c3","revenue":500},
		{"customer_id":"c4","revenue":100}
	]`)
}

func TestJoin_OrderByComputedThenSlice(t *testing.T) {
	d := shop(t)
	// Top 3 line items by total. The unlabelled projection closes the scan
	// scopes, so the computed order sits below it, on the raw join output.
	// Totals: i7=500, i6=450, i3=300, then i8=100, i1/i4/i9=80, ...
	d.Query(q(map[string]protocol.Node{
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"j": {Kind: "join", Left: "i", Right: "p", Join: "inner",
			On: protocol.Eq(protocol.Col("i", "product_id"), protocol.Col("p", "id"))},
		"by_total": {Kind: "order", Input: "j", Terms: []protocol.OrderTerm{
			{Expr: *protocol.Mul(protocol.Col("i", "quantity"), protocol.Col("i", "unit_price")), Desc: true},
		}},
		"top3": {Kind: "slice", Input: "by_total", Limit: new(int(3))},
		"out": {Kind: "project", Input: "top3", Fields: []protocol.Field{
			{As: "item", Expr: *protocol.Col("i", "id")},
			{As: "product", Expr: *protocol.Col("p", "name")},
			{As: "total", Expr: *protocol.Mul(protocol.Col("i", "quantity"), protocol.Col("i", "unit_price"))},
		}},
	}, "out", "many")).Equals(`[
		{"item":"i7","product":"Chair","total":500},
		{"item":"i6","product":"Desk","total":450},
		{"item":"i3","product":"Monitor","total":300}
	]`)
}

func TestJoin_ThreeWayChain(t *testing.T) {
	d := shop(t)
	// items → orders → customers, two join nodes. c4's only order is o6,
	// whose only item is i8.
	d.Query(q(map[string]protocol.Node{
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"io": {Kind: "join", Left: "i", Right: "o", Join: "inner",
			On: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"ioc": {Kind: "join", Left: "io", Right: "c", Join: "inner",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"dee": {Kind: "filter", Input: "ioc",
			Predicate: protocol.Eq(protocol.Col("c", "id"), protocol.Lit("c4"))},
		"out": {Kind: "project", Input: "dee", Fields: []protocol.Field{
			{As: "item", Expr: *protocol.Col("i", "id")},
			{As: "name", Expr: *protocol.Col("c", "name")},
		}},
	}, "out", "many")).Equals(`[{"item":"i8","name":"Dee"}]`)
}

func TestJoin_SelfLeftJoinReferrals(t *testing.T) {
	d := shop(t)
	// Scope labels are query-wide unique, so the self-join binds "a" and "b".
	// c1 and c5 have NULL referrer_id: the ON is UNKNOWN, so they null-pad.
	d.Query(q(map[string]protocol.Node{
		"a": {Kind: "scan", Table: "customers", Scope: "a"},
		"b": {Kind: "scan", Table: "customers", Scope: "b"},
		"j": {Kind: "join", Left: "a", Right: "b", Join: "left",
			On: protocol.Eq(protocol.Col("a", "referrer_id"), protocol.Col("b", "id"))},
		"sorted": {Kind: "order", Input: "j",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("a", "id")}}},
		"out": {Kind: "project", Input: "sorted", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("a", "id")},
			{As: "referrer", Expr: *protocol.Col("b", "name")},
		}},
	}, "out", "many")).Equals(`[
		{"id":"c1","referrer":null},
		{"id":"c2","referrer":"Ada"},
		{"id":"c3","referrer":"Ada"},
		{"id":"c4","referrer":"Bob"},
		{"id":"c5","referrer":null}
	]`)
}

func TestJoin_FilteredScansAsInputs(t *testing.T) {
	d := shop(t)
	// Join inputs are ordinary relations: delivered orders (o1, o3, o6)
	// against gold customers (c1, c3). Only o1 belongs to a gold customer.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"delivered": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("delivered"))},
		"u": {Kind: "scan", Table: "customers", Scope: "u"},
		"gold": {Kind: "filter", Input: "u",
			Predicate: protocol.Eq(protocol.Col("u", "tier"), protocol.Lit("gold"))},
		"j": {Kind: "join", Left: "delivered", Right: "gold", Join: "inner",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("u", "id"))},
		"out": {Kind: "project", Input: "j",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o1"}]`)
}

func TestJoin_LabelledProjectionsAsInputs(t *testing.T) {
	d := shop(t)
	// Labelled projections are joinable relations; their scopes ("lc", "ro")
	// address the join output. Pending orders: o5 → c3, o7 → c1.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"lp": {Kind: "project", Input: "c", Scope: "lc", Fields: []protocol.Field{
			{As: "cid", Expr: *protocol.Col("c", "id")},
			{As: "tier", Expr: *protocol.Col("c", "tier")},
		}},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"rp": {Kind: "project", Input: "o", Scope: "ro", Fields: []protocol.Field{
			{As: "ocust", Expr: *protocol.Col("o", "customer_id")},
			{As: "status", Expr: *protocol.Col("o", "status")},
		}},
		"j": {Kind: "join", Left: "lp", Right: "rp", Join: "inner",
			On: protocol.Eq(protocol.Col("lc", "cid"), protocol.Col("ro", "ocust"))},
		"pending": {Kind: "filter", Input: "j",
			Predicate: protocol.Eq(protocol.Col("ro", "status"), protocol.Lit("pending"))},
		"sorted": {Kind: "order", Input: "pending",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("lc", "cid")}}},
		"out": {Kind: "project", Input: "sorted", Fields: []protocol.Field{
			{As: "cid", Expr: *protocol.Col("lc", "cid")},
			{As: "status", Expr: *protocol.Col("ro", "status")},
		}},
	}, "out", "many")).Equals(`[
		{"cid":"c1","status":"pending"},
		{"cid":"c3","status":"pending"}
	]`)
}

func TestJoin_NonEqualityCondition(t *testing.T) {
	d := shop(t)
	// ON o.placed_at < c.created_at * 10 — a pure theta join. Orders placed
	// strictly before each customer's created_at*10:
	//   c1 (1000): none. c2 (2000): 1000,1500,1600 → 3.
	//   c3 (3000): +2000,2500 → 5. c4 (4000): +3000,3500 → 7. c5 (5000): 7.
	// c1 is absent — an inner join with no matches produces no group.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"j": {Kind: "join", Left: "c", Right: "o", Join: "inner",
			On: protocol.Lt(protocol.Col("o", "placed_at"),
				&protocol.Expr{Kind: "binary", Op: "mul",
					Left: protocol.Col("c", "created_at"), Right: protocol.Lit(10)})},
		"byc": {Kind: "aggregate", Input: "j", Scope: "byc",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("c", "id")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"sorted": {Kind: "order", Input: "byc",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("byc", "id")}}},
	}, "sorted", "many")).Equals(`[
		{"id":"c2","n":3},
		{"id":"c3","n":5},
		{"id":"c4","n":7},
		{"id":"c5","n":7}
	]`)
}

func TestJoin_SpreadBothSidesCollides(t *testing.T) {
	d := shop(t)
	// customers and orders both produce "id": spreading both scopes over a
	// join collides at bind time. Raw join rows need an explicit projection.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"j": {Kind: "join", Left: "c", Right: "o", Join: "inner",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"out": {Kind: "project", Input: "j", Spread: []string{"c", "o"}},
	}, "out", "many")).ExpectStatus(422).ExpectError(`duplicate projection field "id"`)
}

func TestJoin_LeftJoinCountKeepsZeroCustomers(t *testing.T) {
	d := shop(t)
	// count(o.id) — count of the ARGUMENT — skips the null-padded right side,
	// so c5 (no orders) survives the left join with n=0, the SQL-standard
	// left-join-count idiom.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"j": {Kind: "join", Left: "c", Right: "o", Join: "left",
			On: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Col("c", "id"))},
		"agg": {Kind: "aggregate", Input: "j", Scope: "agg",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("c", "id")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", Arg: protocol.Col("o", "id"), As: "n"}}},
		"sorted": {Kind: "order", Input: "agg",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("agg", "id")}}},
	}, "sorted", "many")).Equals(`[
		{"id":"c1","n":3},
		{"id":"c2","n":2},
		{"id":"c3","n":1},
		{"id":"c4","n":1},
		{"id":"c5","n":0}
	]`)
}

func TestJoin_DuplicateScopeRejected(t *testing.T) {
	d := shop(t)
	// Scope labels are query-wide unique — two scans cannot share "x".
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "x"},
		"o": {Kind: "scan", Table: "orders", Scope: "x"},
		"j": {Kind: "join", Left: "c", Right: "o", Join: "inner",
			On: protocol.Eq(protocol.Col("x", "customer_id"), protocol.Col("x", "id"))},
	}, "j", "many")).ExpectError(`duplicate scope "x"`)
}

func TestJoin_CrossingInOnRejected(t *testing.T) {
	d := shop(t)
	// A join condition may not contain a sub-relation crossing — the binder
	// says to filter above the join instead.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"r": {Kind: "scan", Table: "reviews", Scope: "r"},
		"j": {Kind: "join", Left: "c", Right: "o", Join: "inner",
			On: protocol.Exists("r")},
	}, "j", "many")).ExpectError("a join condition cannot contain a sub-relation crossing")
}
