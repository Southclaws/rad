package planner

// Fundamentals: scan, filter, project, order, slice — one relational verb at
// a time, then their plain compositions. Every query is the literal wire
// tree; every expectation is hand-derived from the fixture table in
// fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

func TestBasicFullScan(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
	}, "c", "many")).Equals(`[
		{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null},
		{"id":"c2","name":"Bob","email":"bob@shop.io","tier":"silver","created_at":200,"referrer_id":"c1"},
		{"id":"c3","name":"Cyn","email":"cyn@shop.io","tier":"gold","created_at":300,"referrer_id":"c1"},
		{"id":"c4","name":"Dee","email":"dee@shop.io","tier":"bronze","created_at":400,"referrer_id":"c2"},
		{"id":"c5","name":"Eli","email":"eli@shop.io","tier":"bronze","created_at":500,"referrer_id":null}
	]`)
}

func TestBasicProjectRename(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"gold": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "tier"), protocol.Lit("gold"))},
		"out": {Kind: "project", Input: "gold",
			Fields: []protocol.Field{{As: "customer", Expr: *protocol.Col("c", "name")}}},
	}, "out", "many")).Equals(`[{"customer":"Ada"},{"customer":"Cyn"}]`)
}

func TestBasicComputedField(t *testing.T) {
	d := shop(t)
	// line total = quantity * unit_price (int64 * float64 promotes to float).
	d.Query(q(map[string]protocol.Node{
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"o1_items": {Kind: "filter", Input: "i",
			Predicate: protocol.Eq(protocol.Col("i", "order_id"), protocol.Lit("o1"))},
		"out": {Kind: "project", Input: "o1_items", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("i", "id")},
			{As: "total", Expr: protocol.Expr{Kind: "binary", Op: "mul",
				Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")}},
		}},
	}, "out", "many")).Equals(`[{"id":"i1","total":80},{"id":"i2","total":50}]`)
}

func TestBasicFilterEq(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"pending": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending"))},
		"out": {Kind: "project", Input: "pending",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o5"},{"id":"o7"}]`)
}

func TestBasicFilterNe(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"not_gear": {Kind: "filter", Input: "p",
			Predicate: protocol.Ne(protocol.Col("p", "category"), protocol.Lit("gear"))},
		"out": {Kind: "project", Input: "not_gear",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p5"},{"id":"p6"},{"id":"p7"}]`)
}

func TestBasicRangeBetween(t *testing.T) {
	d := shop(t)
	// 40 <= price < 300.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"mid": {Kind: "filter", Input: "p",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Gte(protocol.Col("p", "price"), protocol.Lit(40)),
				protocol.Lt(protocol.Col("p", "price"), protocol.Lit(300)),
			})},
		"out": {Kind: "project", Input: "mid", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("p", "id")},
			{As: "price", Expr: *protocol.Col("p", "price")},
		}},
	}, "out", "many")).Equals(`[{"id":"p1","price":80},{"id":"p2","price":40},{"id":"p5","price":250}]`)
}

func TestBasicOr(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"upper": {Kind: "filter", Input: "c",
			Predicate: protocol.Or(
				protocol.Eq(protocol.Col("c", "tier"), protocol.Lit("gold")),
				protocol.Eq(protocol.Col("c", "tier"), protocol.Lit("silver")))},
		"out": {Kind: "project", Input: "upper",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"}]`)
}

func TestBasicNotOverOr(t *testing.T) {
	d := shop(t)
	// NOT (cancelled OR pending) over a non-null column = delivered+shipped.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"live": {Kind: "filter", Input: "o",
			Predicate: protocol.Not(protocol.Or(
				protocol.Eq(protocol.Col("o", "status"), protocol.Lit("cancelled")),
				protocol.Eq(protocol.Col("o", "status"), protocol.Lit("pending"))))},
		"out": {Kind: "project", Input: "live",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o2"},{"id":"o3"},{"id":"o6"}]`)
}

func TestBasicOrderDesc(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"newest": {Kind: "order", Input: "o",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"out": {Kind: "project", Input: "newest",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o7"},{"id":"o5"},{"id":"o6"},{"id":"o2"},{"id":"o4"},{"id":"o3"},{"id":"o1"}]`)
}

func TestBasicOrderMultiTerm(t *testing.T) {
	d := shop(t)
	// category ascending, then price descending inside each category.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"sorted": {Kind: "order", Input: "p", Terms: []protocol.OrderTerm{
			{Expr: *protocol.Col("p", "category")},
			{Expr: *protocol.Col("p", "price"), Desc: true},
		}},
		"out": {Kind: "project", Input: "sorted",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p5"},{"id":"p6"},{"id":"p3"},{"id":"p1"},{"id":"p2"},{"id":"p7"}]`)
}

func TestBasicOrderByComputed(t *testing.T) {
	d := shop(t)
	// Top 3 line items by quantity * unit_price.
	d.Query(q(map[string]protocol.Node{
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"by_total": {Kind: "order", Input: "i", Terms: []protocol.OrderTerm{
			{Expr: protocol.Expr{Kind: "binary", Op: "mul",
				Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")}, Desc: true},
		}},
		"top3": {Kind: "slice", Input: "by_total", Limit: new(int(3))},
		"out": {Kind: "project", Input: "top3",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("i", "id")}}},
	}, "out", "many")).Equals(`[{"id":"i7"},{"id":"i6"},{"id":"i3"}]`)
}

func TestBasicPagination(t *testing.T) {
	d := shop(t)
	// Page 2 of orders by placed_at, page size 2: o1,o3 | o4,o2 | ...
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"chrono": {Kind: "order", Input: "o",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at")}}},
		"page2": {Kind: "slice", Input: "chrono", Offset: new(int(2)), Limit: new(int(2))},
		"out": {Kind: "project", Input: "page2",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o4"},{"id":"o2"}]`)
}

func TestBasicLimitZero(t *testing.T) {
	d := shop(t)
	// Limit 0 is explicit: zero rows, not "no limit".
	d.Query(q(map[string]protocol.Node{
		"c":    {Kind: "scan", Table: "customers", Scope: "c"},
		"none": {Kind: "slice", Input: "c", Limit: new(int(0))},
	}, "none", "many")).Empty()
}

func TestBasicOffsetPastEnd(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c":      {Kind: "scan", Table: "customers", Scope: "c"},
		"beyond": {Kind: "slice", Input: "c", Offset: new(int(100))},
	}, "beyond", "many")).Empty()
}

func TestBasicSpreadPlusComputed(t *testing.T) {
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"bronze": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "tier"), protocol.Lit("bronze"))},
		"out": {Kind: "project", Input: "bronze", Spread: []string{"c"},
			Fields: []protocol.Field{{As: "referred", Expr: *protocol.IsNotNull(protocol.Col("c", "referrer_id"))}}},
	}, "out", "many")).Equals(`[
		{"id":"c4","name":"Dee","email":"dee@shop.io","tier":"bronze","created_at":400,"referrer_id":"c2","referred":true},
		{"id":"c5","name":"Eli","email":"eli@shop.io","tier":"bronze","created_at":500,"referrer_id":null,"referred":false}
	]`)
}

func TestBasicStackedFilters(t *testing.T) {
	d := shop(t)
	// Two filter nodes stacked — same meaning as one conjunction.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"gear": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "category"), protocol.Lit("gear"))},
		"pricey": {Kind: "filter", Input: "gear",
			Predicate: protocol.Gt(protocol.Col("p", "price"), protocol.Lit(50))},
		"out": {Kind: "project", Input: "pricey",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p1"},{"id":"p3"}]`)
}

func TestBasicBareBoolPredicate(t *testing.T) {
	d := shop(t)
	// A bool column IS a predicate.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"dead": {Kind: "filter", Input: "p",
			Predicate: protocol.Col("p", "discontinued")},
		"out": {Kind: "project", Input: "dead",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p5"}]`)
}

func TestBasicScanIsPrimaryKeyOrder(t *testing.T) {
	d := shop(t)
	// A bare scan arrives in primary-key order — the one physical order the
	// storage engine guarantees.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"out": {Kind: "project", Input: "o",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o2"},{"id":"o3"},{"id":"o4"},{"id":"o5"},{"id":"o6"},{"id":"o7"}]`)
}
