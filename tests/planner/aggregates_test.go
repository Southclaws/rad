package planner

// Aggregation: global folds, grouped folds, HAVING-style filters above a
// labelled aggregate, folds over joins/projections/empty sets, and fold
// typing. Every query is the literal wire tree; every expectation is
// hand-derived from the fixture table in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

func TestAggGlobalCount(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 5 customers: c1..c5.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"fold": {Kind: "aggregate", Input: "c", Aggs: []protocol.AggTerm{
			{Fn: "count", As: "n"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"n":5}]`)
}

func TestAggCountRowsVsCountNonNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 7 orders; discount is non-NULL only on o2 (10.0) and o5 (25.0).
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"fold": {Kind: "aggregate", Input: "o", Aggs: []protocol.AggTerm{
			{Fn: "count", As: "rows"},
			{Fn: "count", Arg: protocol.Col("o", "discount"), As: "with_discount"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"rows":7,"with_discount":2}]`)
}

func TestAggIntFolds(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// ratings: 5, 4, 2, 5, 1 → sum 17, min 1, max 5, avg 3.4 (avg is float).
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "scan", Table: "reviews", Scope: "r"},
		"fold": {Kind: "aggregate", Input: "r", Aggs: []protocol.AggTerm{
			{Fn: "sum", Arg: protocol.Col("r", "rating"), As: "total"},
			{Fn: "min", Arg: protocol.Col("r", "rating"), As: "lo"},
			{Fn: "max", Arg: protocol.Col("r", "rating"), As: "hi"},
			{Fn: "avg", Arg: protocol.Col("r", "rating"), As: "mean"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"total":17,"lo":1,"hi":5,"mean":3.4}]`)
}

func TestAggFloatSum(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// prices: 80+40+300+450+250+35+5 = 1160.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"fold": {Kind: "aggregate", Input: "p", Aggs: []protocol.AggTerm{
			{Fn: "sum", Arg: protocol.Col("p", "price"), As: "total"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"total":1160}]`)
}

func TestAggGroupByStatus(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// delivered o1,o3,o6 = 3; shipped o2 = 1; pending o5,o7 = 2; cancelled o4 = 1.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"st": {Kind: "aggregate", Input: "o", Scope: "st",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "status")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"by_status": {Kind: "order", Input: "st",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("st", "status")}}},
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
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"st": {Kind: "aggregate", Input: "o", Scope: "st",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "status")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"having": {Kind: "filter", Input: "st",
			Predicate: protocol.Gte(protocol.Col("st", "n"), protocol.Lit(2))},
		"by_status": {Kind: "order", Input: "having",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("st", "status")}}},
	}, "by_status", "many")).Equals(`[
		{"status":"delivered","n":3},
		{"status":"pending","n":2}
	]`)
}

func TestAggOrderByCountDesc(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// count desc, status asc breaks the cancelled/shipped tie at 1.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"st": {Kind: "aggregate", Input: "o", Scope: "st",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "status")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"ranked": {Kind: "order", Input: "st", Terms: []protocol.OrderTerm{
			{Expr: *protocol.Col("st", "n"), Desc: true},
			{Expr: *protocol.Col("st", "status")},
		}},
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
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"st": {Kind: "aggregate", Input: "p", Scope: "st",
			Groups: []protocol.GroupTerm{
				{Expr: *protocol.Col("p", "category")},
				{Expr: *protocol.Col("p", "discontinued")},
			},
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"sorted": {Kind: "order", Input: "st", Terms: []protocol.OrderTerm{
			{Expr: *protocol.Col("st", "category")},
			{Expr: *protocol.Col("st", "discontinued")},
		}},
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
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"st": {Kind: "aggregate", Input: "p", Scope: "st",
			Groups: []protocol.GroupTerm{
				{As: "expensive", Expr: *protocol.Gte(protocol.Col("p", "price"), protocol.Lit(100))},
			},
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"sorted": {Kind: "order", Input: "st",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("st", "expensive")}}},
	}, "sorted", "many")).Equals(`[
		{"expensive":false,"n":4},
		{"expensive":true,"n":3}
	]`)
}

func TestAggOverFilteredInput(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// gear prices: 80, 40, 300 → avg (80+40+300)/3 = 140.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"gear": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "category"), protocol.Lit("gear"))},
		"fold": {Kind: "aggregate", Input: "gear", Aggs: []protocol.AggTerm{
			{Fn: "avg", Arg: protocol.Col("p", "price"), As: "avg_price"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"avg_price":140}]`)
}

func TestAggOverJoin(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Revenue per status, sum(quantity*unit_price) over orders ⋈ items:
	// cancelled o4 = 450; delivered o1 130 + o3 115 + o6 100 = 345;
	// pending o5 500 + o7 120 = 620; shipped o2 = 300.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"lines": {Kind: "join", Left: "o", Right: "i", Join: "inner",
			On: protocol.Eq(protocol.Col("i", "order_id"), protocol.Col("o", "id"))},
		"rev": {Kind: "aggregate", Input: "lines", Scope: "rev",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "status")}},
			Aggs: []protocol.AggTerm{
				{Fn: "sum", Arg: &protocol.Expr{Kind: "binary", Op: "mul",
					Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")}, As: "revenue"},
			}},
		"by_status": {Kind: "order", Input: "rev",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("rev", "status")}}},
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
	d.Query(q(map[string]protocol.Node{
		"i": {Kind: "scan", Table: "order_items", Scope: "i"},
		"lines": {Kind: "project", Input: "i", Scope: "lines",
			Fields: []protocol.Field{
				{As: "t", Expr: protocol.Expr{Kind: "binary", Op: "mul",
					Left: protocol.Col("i", "quantity"), Right: protocol.Col("i", "unit_price")}},
			}},
		"fold": {Kind: "aggregate", Input: "lines", Aggs: []protocol.AggTerm{
			{Fn: "sum", Arg: protocol.Col("lines", "t"), As: "total"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"total":1715}]`)
}

func TestAggFoldsOverEmptySet(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// count of nothing is 0; every other fold over nothing is NULL.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"none": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("ghost"))},
		"fold": {Kind: "aggregate", Input: "none", Aggs: []protocol.AggTerm{
			{Fn: "count", As: "n"},
			{Fn: "avg", Arg: protocol.Col("o", "placed_at"), As: "avg_placed"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"n":0,"avg_placed":null}]`)
}

func TestAggTopGroupsBySlice(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Top 2 statuses by count desc (status asc breaks the 1-count tie
	// below the cut): delivered 3, pending 2.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"st": {Kind: "aggregate", Input: "o", Scope: "st",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("o", "status")}},
			Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
		"ranked": {Kind: "order", Input: "st", Terms: []protocol.OrderTerm{
			{Expr: *protocol.Col("st", "n"), Desc: true},
			{Expr: *protocol.Col("st", "status")},
		}},
		"top2": {Kind: "slice", Input: "ranked", Limit: new(int(2))},
	}, "top2", "many")).Equals(`[
		{"status":"delivered","n":3},
		{"status":"pending","n":2}
	]`)
}

func TestAggMinMaxOverText(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// names: Ada, Bob, Cyn, Dee, Eli → min "Ada", max "Eli".
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"fold": {Kind: "aggregate", Input: "c", Aggs: []protocol.AggTerm{
			{Fn: "min", Arg: protocol.Col("c", "name"), As: "first_name"},
			{Fn: "max", Arg: protocol.Col("c", "name"), As: "last_name"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"first_name":"Ada","last_name":"Eli"}]`)
}

func TestAggSumInt64StaysInteger(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// stock: 12+0+5+2+0+40+100 = 159 — sum over int64 stays integer.
	// (avg over int64 being float is covered by TestAggIntFolds: 3.4.)
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"fold": {Kind: "aggregate", Input: "p", Aggs: []protocol.AggTerm{
			{Fn: "sum", Arg: protocol.Col("p", "stock"), As: "total_stock"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"total_stock":159}]`)
}
