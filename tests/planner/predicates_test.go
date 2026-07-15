package planner

// Predicate semantics: three-valued logic over nullable columns, literal
// coercion at type boundaries, casts, arithmetic (NULL propagation, division
// by zero, negation), and predicate type errors. Every query is the literal
// wire tree; every expectation is hand-derived from the fixture table in
// fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// -
// three-valued logic
// -

func TestPredEqNullLiteralMatchesNothing(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// referrer_id = NULL is UNKNOWN for every row — including the rows where
	// referrer_id IS NULL. Nothing matches; is_null is the only door.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"eq_null": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "referrer_id"), lirwire.LitOf(nil))),
	}, "eq_null", "many")).Empty()
}

func TestPredNeSkipsNullRows(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// referrer_id ne 'c1': c1/c5 have NULL referrers (UNKNOWN, filtered),
	// c2/c3 are referred by c1 (FALSE) — only c4 (referred by c2) remains.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"not_c1": lirwire.Filter("c",
			lirwire.Binary("ne", lirwire.Col("c", "referrer_id"), lirwire.LitOf("c1"))),
		"out": lirwire.Project("not_c1", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c4"}]`)
}

func TestPredNotEqSkipsNullRows(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// NOT (referrer_id = 'c1') is the same set as ne: NOT UNKNOWN is UNKNOWN,
	// so the NULL-referrer rows stay filtered out.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"not_c1": lirwire.Filter("c",
			lirwire.Unary("not", lirwire.Binary("eq", lirwire.Col("c", "referrer_id"), lirwire.LitOf("c1")))),
		"out": lirwire.Project("not_c1", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c4"}]`)
}

func TestPredIsNullMatchesOnlyNulls(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Orders with no discount: o1, o3, o4, o6, o7.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"no_discount": lirwire.Filter("o",
			lirwire.Unary("is_null", lirwire.Col("o", "discount"))),
		"by_id": lirwire.Order("no_discount",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o3"},{"id":"o4"},{"id":"o6"},{"id":"o7"}]`)
}

func TestPredIsNotNullMatchesOnlyPresent(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Orders with a discount: o2 (10.0), o5 (25.0).
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"discounted": lirwire.Filter("o",
			lirwire.Unary("is_not_null", lirwire.Col("o", "discount"))),
		"by_id": lirwire.Order("discounted",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o2"},{"id":"o5"}]`)
}

func TestPredDeMorganAgreesOverNulls(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// a = (referrer_id = 'c1'), b = (referrer_id = 'c2').
	// Every customer either has a referrer in {c1, c2} (predicate FALSE) or a
	// NULL referrer (UNKNOWN) — so NOT(a OR b) keeps nothing, and De Morgan's
	// equivalent AND(NOT a, NOT b) must agree exactly.
	a := func() lirwire.Expr {
		return lirwire.Binary("eq", lirwire.Col("c", "referrer_id"), lirwire.LitOf("c1"))
	}
	b := func() lirwire.Expr {
		return lirwire.Binary("eq", lirwire.Col("c", "referrer_id"), lirwire.LitOf("c2"))
	}

	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"neither": lirwire.Filter("c",
			lirwire.Unary("not", lirwire.Binary("or", a(), b()))),
	}, "neither", "many")).Empty()

	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"neither": lirwire.Filter("c",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Unary("not", a()),
				lirwire.Unary("not", b()),
			})),
	}, "neither", "many")).Empty()
}

// -
// type boundaries: coercion and casts
// -

func TestPredFloatLiteralAgainstIntColumnRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// FINDING: a fractional literal against an int64 column is rejected at
	// bind time (422) — cross-type comparison requires an explicit cast.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"frac": lirwire.Filter("p",
			lirwire.Binary("gt", lirwire.Col("p", "stock"), lirwire.LitOf(1.5))),
	}, "frac", "many")).ExpectStatus(422).ExpectError("expected an int64 value")
}

func TestPredCastIntColumnToFloatComparesAgainstFraction(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The escape hatch for the previous test: cast(stock to float64) > 1.5
	// keeps products with stock >= 2 — p1(12), p3(5), p4(2), p6(40), p7(100).
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"stocked": lirwire.Filter("p",
			lirwire.Binary("gt",
				lirwire.Cast(lirwire.Col("p", "stock"), "float64"),
				lirwire.LitOf(1.5))),
		"by_id": lirwire.Order("stocked",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p1"},{"id":"p3"},{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

func TestPredCastFloatToIntTruncates(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// cast(price to int64) over category 'paper': p7 price 5.0 → 5.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"paper": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "category"), lirwire.LitOf("paper"))),
		"out": lirwire.Project("paper", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "price_int", Expr: lirwire.Cast(lirwire.Col("p", "price"), "int64")},
		}),
	}, "out", "many")).Equals(`[{"id":"p7","price_int":5}]`)
}

// -
// arithmetic
// -

func TestPredArithmeticPropagatesNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// discount + 5 over o1 (NULL discount) and o2 (10.0): NULL in, NULL out.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"pair": lirwire.Filter("o",
			lirwire.Binary("or",
				lirwire.Binary("eq", lirwire.Col("o", "id"), lirwire.LitOf("o1")),
				lirwire.Binary("eq", lirwire.Col("o", "id"), lirwire.LitOf("o2")))),
		"by_id": lirwire.Order("pair",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "bumped", Expr: lirwire.Binary("add", lirwire.Col("o", "discount"), lirwire.LitOf(5))},
		}),
	}, "out", "many")).Equals(`[{"id":"o1","bumped":null},{"id":"o2","bumped":15}]`)
}

func TestPredDivisionByZeroErrors(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// p2 has stock 0; price / stock must fail loudly, not yield NULL or Inf.
	// FINDING: runtime evaluation errors surface as HTTP 422.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"mouse": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "id"), lirwire.LitOf("p2"))),
		"out": lirwire.Project("mouse", "", nil, []lirwire.Field{
			{As: "ratio", Expr: lirwire.Binary("div", lirwire.Col("p", "price"), lirwire.Col("p", "stock"))},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectCode("execution_failed").ExpectError("division by zero")
}

func TestPredNestedArithmetic(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// (price - 10) * 2 for p2 (price 40.0) → 60.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"mouse": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "id"), lirwire.LitOf("p2"))),
		"out": lirwire.Project("mouse", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "adjusted", Expr: lirwire.Binary("mul",
				lirwire.Binary("sub", lirwire.Col("p", "price"), lirwire.LitOf(10)),
				lirwire.LitOf(2))},
		}),
	}, "out", "many")).Equals(`[{"id":"p2","adjusted":60}]`)
}

func TestPredNegate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// -price for p7 (price 5.0) → -5.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"notebook": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "id"), lirwire.LitOf("p7"))),
		"out": lirwire.Project("notebook", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "neg_price", Expr: lirwire.Unary("negate", lirwire.Col("p", "price"))},
		}),
	}, "out", "many")).Equals(`[{"id":"p7","neg_price":-5}]`)
}

// -
// comparisons over other scalar types
// -

func TestPredBoolColumnEqualsLiteral(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// discontinued = false → everything except p5.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"alive": lirwire.Filter("p",
			lirwire.Binary("eq", lirwire.Col("p", "discontinued"), lirwire.LitOf(false))),
		"by_id": lirwire.Order("alive",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[{"id":"p1"},{"id":"p2"},{"id":"p3"},{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

func TestPredTextRange(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 'B' <= name < 'D' over customers: Bob (c2), Cyn (c3).
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"mid": lirwire.Filter("c",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("gte", lirwire.Col("c", "name"), lirwire.LitOf("B")),
				lirwire.Binary("lt", lirwire.Col("c", "name"), lirwire.LitOf("D")),
			})),
		"by_id": lirwire.Order("mid",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "name", Expr: lirwire.Col("c", "name")}}),
	}, "out", "many")).Equals(`[{"name":"Bob"},{"name":"Cyn"}]`)
}

func TestPredOrOverNullableColumn(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// discount = 10 OR discount = 25: NULL discounts make both sides UNKNOWN
	// (UNKNOWN ∨ UNKNOWN = UNKNOWN, filtered) — only o2 and o5 match.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"known": lirwire.Filter("o",
			lirwire.Binary("or",
				lirwire.Binary("eq", lirwire.Col("o", "discount"), lirwire.LitOf(10)),
				lirwire.Binary("eq", lirwire.Col("o", "discount"), lirwire.LitOf(25)))),
		"by_id": lirwire.Order("known",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o2"},{"id":"o5"}]`)
}

// -
// predicate type errors
// -

func TestPredNotOverNonBoolRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// not(name) — negating a text column is a bind-time type error (422),
	// not a coercion.
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"bad": lirwire.Filter("c",
			lirwire.Unary("not", lirwire.Col("c", "name"))),
	}, "bad", "many")).ExpectStatus(422).ExpectError("not needs a boolean")
}
