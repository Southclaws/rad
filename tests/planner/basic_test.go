package planner

// Fundamentals: scan, filter, project, order, slice — one relational verb at
// a time, then their plain compositions. Every query is the literal wire
// tree; every expectation is hand-derived from the fixture table in
// fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

func TestBasicFullScan(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
	}, "c", "many")).Equals(`[
		{"id":"c1","name":"Ada","email":"ada@shop.io","tier":"gold","created_at":100,"referrer_id":null},
		{"id":"c2","name":"Bob","email":"bob@shop.io","tier":"silver","created_at":200,"referrer_id":"c1"},
		{"id":"c3","name":"Cyn","email":"cyn@shop.io","tier":"gold","created_at":300,"referrer_id":"c1"},
		{"id":"c4","name":"Dee","email":"dee@shop.io","tier":"bronze","created_at":400,"referrer_id":"c2"},
		{"id":"c5","name":"Eli","email":"eli@shop.io","tier":"bronze","created_at":500,"referrer_id":null}
	]`)
}

func TestBasicProjectRename(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"gold": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold"))),
		"out": lirwire.Project("gold", "", nil,
			[]lirwire.Field{{As: "customer", Expr: lirwire.Col("c", "name")}}),
	}, "out", "many")).Equals(`[{"customer":"Ada"},{"customer":"Cyn"}]`)
}

func TestBasicComputedField(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// line total = quantity * unit_price (int64 * float64 promotes to float).
	d.Query(q(map[string]lirwire.Node{
		"i": lirwire.Scan("order_items", "i"),
		"o1_items": lirwire.Filter("i",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.LitOf("o1"))),
		"out": lirwire.Project("o1_items", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("i", "id")},
			{As: "total", Expr: lirwire.Binary("mul",
				lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))},
		}),
	}, "out", "many")).Equals(`[{"id":"i1","total":80},{"id":"i2","total":50}]`)
}

func TestBasicFilterEq(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"pending": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))),
		"out": lirwire.Project("pending", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o5"},{"id":"o7"}]`)
}

func TestBasicFilterNe(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"not_gear": lirwire.Filter("p",
			lirwire.Binary("ne", lirwire.Col("p", "category"), lirwire.LitOf("gear"))),
		"out": lirwire.Project("not_gear", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p5"},{"id":"p6"},{"id":"p7"}]`)
}

func TestBasicRangeBetween(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 40 <= price < 300.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"mid": lirwire.Filter("p",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("gte", lirwire.Col("p", "price"), lirwire.LitOf(40)),
				lirwire.Binary("lt", lirwire.Col("p", "price"), lirwire.LitOf(300)),
			})),
		"out": lirwire.Project("mid", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "price", Expr: lirwire.Col("p", "price")},
		}),
	}, "out", "many")).Equals(`[{"id":"p1","price":80},{"id":"p2","price":40},{"id":"p5","price":250}]`)
}

func TestBasicOr(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"upper": lirwire.Filter("c",
			lirwire.Binary("or",
				lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold")),
				lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("silver")))),
		"out": lirwire.Project("upper", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"}]`)
}

func TestBasicNotOverOr(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// NOT (cancelled OR pending) over a non-null column = delivered+shipped.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"live": lirwire.Filter("o",
			lirwire.Unary("not", lirwire.Binary("or",
				lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("cancelled")),
				lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("pending"))))),
		"out": lirwire.Project("live", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o2"},{"id":"o3"},{"id":"o6"}]`)
}

func TestBasicOrderDesc(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"newest": lirwire.Order("o",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"out": lirwire.Project("newest", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o7"},{"id":"o5"},{"id":"o6"},{"id":"o2"},{"id":"o4"},{"id":"o3"},{"id":"o1"}]`)
}

func TestBasicOrderMultiTerm(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// category ascending, then price descending inside each category.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"sorted": lirwire.Order("p", []lirwire.OrderTerm{
			{Expr: lirwire.Col("p", "category")},
			{Expr: lirwire.Col("p", "price"), Desc: ptrBool(true)},
		}),
		"out": lirwire.Project("sorted", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p4"},{"id":"p5"},{"id":"p6"},{"id":"p3"},{"id":"p1"},{"id":"p2"},{"id":"p7"}]`)
}

func TestBasicOrderByComputed(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Top 3 line items by quantity * unit_price.
	d.Query(q(map[string]lirwire.Node{
		"i": lirwire.Scan("order_items", "i"),
		"by_total": lirwire.Order("i", []lirwire.OrderTerm{
			{Expr: lirwire.Binary("mul",
				lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price")), Desc: ptrBool(true)},
		}),
		"top3": lirwire.Slice("by_total", 0, ptrInt(3)),
		"out": lirwire.Project("top3", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("i", "id")}}),
	}, "out", "many")).Equals(`[{"id":"i7"},{"id":"i6"},{"id":"i3"}]`)
}

func TestBasicPagination(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Page 2 of orders by placed_at, page size 2: o1,o3 | o4,o2 | ...
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"chrono": lirwire.Order("o",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at")}}),
		"page2": lirwire.Slice("chrono", 2, ptrInt(2)),
		"out": lirwire.Project("page2", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o4"},{"id":"o2"}]`)
}

func TestBasicLimitZero(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Limit 0 is explicit: zero rows, not "no limit".
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"none": lirwire.Slice("c", 0, ptrInt(0)),
	}, "none", "many")).Empty()
}

func TestBasicOffsetPastEnd(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":      lirwire.Scan("customers", "c"),
		"beyond": lirwire.Slice("c", 100, nil),
	}, "beyond", "many")).Empty()
}

func TestBasicSpreadPlusComputed(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"bronze": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("bronze"))),
		"out": lirwire.Project("bronze", "", []string{"c"},
			[]lirwire.Field{{As: "referred", Expr: lirwire.Unary("is_not_null", lirwire.Col("c", "referrer_id"))}}),
	}, "out", "many")).Equals(`[
		{"id":"c4","name":"Dee","email":"dee@shop.io","tier":"bronze","created_at":400,"referrer_id":"c2","referred":true},
		{"id":"c5","name":"Eli","email":"eli@shop.io","tier":"bronze","created_at":500,"referrer_id":null,"referred":false}
	]`)
}

func TestBasicStackedFilters(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Two filter nodes stacked — same meaning as one conjunction.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"gear": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "category"), lirwire.LitOf("gear"))),
		"pricey": lirwire.Filter("gear",
			lirwire.Binary("gt", lirwire.Col("p", "price"), lirwire.LitOf(50))),
		"out": lirwire.Project("pricey", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p1"},{"id":"p3"}]`)
}

func TestBasicBareBoolPredicate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A bool column IS a predicate.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"dead": lirwire.Filter("p",
			lirwire.Col("p", "discontinued")),
		"out": lirwire.Project("dead", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p5"}]`)
}

func TestBasicScanIsPrimaryKeyOrder(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A bare scan arrives in primary-key order — the one physical order the
	// storage engine guarantees.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"out": lirwire.Project("o", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o2"},{"id":"o3"},{"id":"o4"},{"id":"o5"},{"id":"o6"},{"id":"o7"}]`)
}
