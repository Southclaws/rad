package planner

// Aggregation: global folds, grouped folds, HAVING-style filters above a
// labelled aggregate, folds over joins/projections/empty sets, and fold
// typing. Every query is the literal wire tree; every expectation is
// hand-derived from the fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

func TestAggGlobalCount(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 5 customers: c1..c5.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"fold": lirwire.Aggregate("c", "", nil, []lirwire.AggTerm{
			{Fn: "count", As: "n"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"n":5}]`)
}

func TestAggCountRowsVsCountNonNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 7 orders; discount is non-NULL only on o2 (10.0) and o5 (25.0).
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"fold": lirwire.Aggregate("o", "", nil, []lirwire.AggTerm{
			{Fn: "count", As: "rows"},
			{Fn: "count", Arg: ptrExpr(lirwire.Col("o", "discount")), As: "with_discount"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"rows":7,"with_discount":2}]`)
}

func TestAggIntFolds(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// ratings: 5, 4, 2, 5, 1 → sum 17, min 1, max 5, avg 3.4 (avg is float).
	d.Query(q(map[string]lirwire.Node{
		"r": lirwire.Scan("reviews", "r"),
		"fold": lirwire.Aggregate("r", "", nil, []lirwire.AggTerm{
			{Fn: "sum", Arg: ptrExpr(lirwire.Col("r", "rating")), As: "total"},
			{Fn: "min", Arg: ptrExpr(lirwire.Col("r", "rating")), As: "lo"},
			{Fn: "max", Arg: ptrExpr(lirwire.Col("r", "rating")), As: "hi"},
			{Fn: "avg", Arg: ptrExpr(lirwire.Col("r", "rating")), As: "mean"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"total":17,"lo":1,"hi":5,"mean":3.4}]`)
}

func TestAggFloatSum(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// prices: 80+40+300+450+250+35+5 = 1160.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"fold": lirwire.Aggregate("p", "", nil, []lirwire.AggTerm{
			{Fn: "sum", Arg: ptrExpr(lirwire.Col("p", "price")), As: "total"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"total":1160}]`)
}

func TestAggGroupByStatus(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// delivered o1,o3,o6 = 3; shipped o2 = 1; pending o5,o7 = 2; cancelled o4 = 1.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"st": lirwire.Aggregate("o", "st",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "status")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"by_status": lirwire.Order("st",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("st", "status")}}),
	}, "by_status", "many")).Equals(`[
		{"status":"cancelled","n":1},
		{"status":"delivered","n":3},
		{"status":"pending","n":2},
		{"status":"shipped","n":1}
	]`)
}

func TestAggHavingCountAtLeastTwo(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// HAVING n >= 2: a filter above the labelled aggregate — keeps
	// delivered (3) and pending (2), drops cancelled (1) and shipped (1).
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"st": lirwire.Aggregate("o", "st",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "status")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"having": lirwire.Filter("st",
			lirwire.Binary("gte", lirwire.Col("st", "n"), lirwire.LitOf(2))),
		"by_status": lirwire.Order("having",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("st", "status")}}),
	}, "by_status", "many")).Equals(`[
		{"status":"delivered","n":3},
		{"status":"pending","n":2}
	]`)
}

func TestAggOrderByCountDesc(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// count desc, status asc breaks the cancelled/shipped tie at 1.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"st": lirwire.Aggregate("o", "st",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "status")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"ranked": lirwire.Order("st", []lirwire.OrderTerm{
			{Expr: lirwire.Col("st", "n"), Desc: ptrBool(true)},
			{Expr: lirwire.Col("st", "status")},
		}),
	}, "ranked", "many")).Equals(`[
		{"status":"delivered","n":3},
		{"status":"pending","n":2},
		{"status":"cancelled","n":1},
		{"status":"shipped","n":1}
	]`)
}

func TestAggGroupByTwoColumns(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// (category, discontinued): furniture/false p4,p6 = 2; furniture/true p5 = 1;
	// gear/false p1,p2,p3 = 3; paper/false p7 = 1.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"st": lirwire.Aggregate("p", "st",
			[]lirwire.GroupTerm{
				{Expr: lirwire.Col("p", "category")},
				{Expr: lirwire.Col("p", "discontinued")},
			},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"sorted": lirwire.Order("st", []lirwire.OrderTerm{
			{Expr: lirwire.Col("st", "category")},
			{Expr: lirwire.Col("st", "discontinued")},
		}),
	}, "sorted", "many")).Equals(`[
		{"category":"furniture","discontinued":false,"n":2},
		{"category":"furniture","discontinued":true,"n":1},
		{"category":"gear","discontinued":false,"n":3},
		{"category":"paper","discontinued":false,"n":1}
	]`)
}

func TestAggGroupByComputedExpression(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// expensive = price >= 100: false p1,p2,p6,p7 = 4; true p3,p4,p5 = 3.
	// A computed group expression requires As.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"st": lirwire.Aggregate("p", "st",
			[]lirwire.GroupTerm{
				{As: ptrStr("expensive"), Expr: lirwire.Binary("gte", lirwire.Col("p", "price"), lirwire.LitOf(100))},
			},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"sorted": lirwire.Order("st",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("st", "expensive")}}),
	}, "sorted", "many")).Equals(`[
		{"expensive":false,"n":4},
		{"expensive":true,"n":3}
	]`)
}

func TestAggOverFilteredInput(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// gear prices: 80, 40, 300 → avg (80+40+300)/3 = 140.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"gear": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "category"), lirwire.LitOf("gear"))),
		"fold": lirwire.Aggregate("gear", "", nil, []lirwire.AggTerm{
			{Fn: "avg", Arg: ptrExpr(lirwire.Col("p", "price")), As: "avg_price"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"avg_price":140}]`)
}

func TestAggOverJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Revenue per status, sum(quantity*unit_price) over orders ⋈ items:
	// cancelled o4 = 450; delivered o1 130 + o3 115 + o6 100 = 345;
	// pending o5 500 + o7 120 = 620; shipped o2 = 300.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"i": lirwire.Scan("order_items", "i"),
		"lines": lirwire.Join("o", "i", "inner",
			lirwire.Binary("eq", lirwire.Col("i", "order_id"), lirwire.Col("o", "id"))),
		"rev": lirwire.Aggregate("lines", "rev",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "status")}},
			[]lirwire.AggTerm{
				{Fn: "sum", Arg: ptrExpr(lirwire.Binary("mul", lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))), As: "revenue"},
			}),
		"by_status": lirwire.Order("rev",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("rev", "status")}}),
	}, "by_status", "many")).Equals(`[
		{"status":"cancelled","revenue":450},
		{"status":"delivered","revenue":345},
		{"status":"pending","revenue":620},
		{"status":"shipped","revenue":300}
	]`)
}

func TestAggOverProjection(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Line totals: i1 80, i2 50, i3 300, i4 80, i5 35, i6 450, i7 500,
	// i8 100, i9 80, i10 40 → sum 1715.
	d.Query(q(map[string]lirwire.Node{
		"i": lirwire.Scan("order_items", "i"),
		"lines": lirwire.Project("i", "lines", nil, []lirwire.Field{
			{As: "t", Expr: lirwire.Binary("mul", lirwire.Col("i", "quantity"), lirwire.Col("i", "unit_price"))},
		}),
		"fold": lirwire.Aggregate("lines", "", nil, []lirwire.AggTerm{
			{Fn: "sum", Arg: ptrExpr(lirwire.Col("lines", "t")), As: "total"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"total":1715}]`)
}

func TestAggFoldsOverEmptySet(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// count of nothing is 0; every other fold over nothing is NULL.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"none": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("ghost"))),
		"fold": lirwire.Aggregate("none", "", nil, []lirwire.AggTerm{
			{Fn: "count", As: "n"},
			{Fn: "avg", Arg: ptrExpr(lirwire.Col("o", "placed_at")), As: "avg_placed"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"n":0,"avg_placed":null}]`)
}

func TestAggTopGroupsBySlice(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Top 2 statuses by count desc (status asc breaks the 1-count tie
	// below the cut): delivered 3, pending 2.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"st": lirwire.Aggregate("o", "st",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("o", "status")}},
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"ranked": lirwire.Order("st", []lirwire.OrderTerm{
			{Expr: lirwire.Col("st", "n"), Desc: ptrBool(true)},
			{Expr: lirwire.Col("st", "status")},
		}),
		"top2": lirwire.Slice("ranked", 0, ptrInt(2)),
	}, "top2", "many")).Equals(`[
		{"status":"delivered","n":3},
		{"status":"pending","n":2}
	]`)
}

func TestAggMinMaxOverText(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// names: Ada, Bob, Cyn, Dee, Eli → min "Ada", max "Eli".
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"fold": lirwire.Aggregate("c", "", nil, []lirwire.AggTerm{
			{Fn: "min", Arg: ptrExpr(lirwire.Col("c", "name")), As: "first_name"},
			{Fn: "max", Arg: ptrExpr(lirwire.Col("c", "name")), As: "last_name"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"first_name":"Ada","last_name":"Eli"}]`)
}

func TestAggSumInt64StaysInteger(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// stock: 12+0+5+2+0+40+100 = 159 — sum over int64 stays integer.
	// (avg over int64 being float is covered by TestAggIntFolds: 3.4.)
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"fold": lirwire.Aggregate("p", "", nil, []lirwire.AggTerm{
			{Fn: "sum", Arg: ptrExpr(lirwire.Col("p", "stock")), As: "total_stock"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"total_stock":159}]`)
}
