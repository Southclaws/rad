package planner

// Predicate semantics: three-valued logic over nullable columns, literal
// coercion at type boundaries, casts, arithmetic (NULL propagation, division
// by zero, negation), and predicate type errors. Every query is the literal
// wire tree; every expectation is hand-derived from the fixture table in
// fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

// ── three-valued logic ──────────────────────────────────────────────────────

func TestPredEqNullLiteralMatchesNothing(t *testing.T) {
	d := shop(t)
	// referrer_id = NULL is UNKNOWN for every row — including the rows where
	// referrer_id IS NULL. Nothing matches; is_null is the only door.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"eq_null": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "referrer_id"), protocol.Lit(nil))},
	}, "eq_null", "many")).Empty()
}

func TestPredNeSkipsNullRows(t *testing.T) {
	d := shop(t)
	// referrer_id ne 'c1': c1/c5 have NULL referrers (UNKNOWN, filtered),
	// c2/c3 are referred by c1 (FALSE) — only c4 (referred by c2) remains.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"not_c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Ne(protocol.Col("c", "referrer_id"), protocol.Lit("c1"))},
		"out": {Kind: "project", Input: "not_c1",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c4"}]`)
}

func TestPredNotEqSkipsNullRows(t *testing.T) {
	d := shop(t)
	// NOT (referrer_id = 'c1') is the same set as ne: NOT UNKNOWN is UNKNOWN,
	// so the NULL-referrer rows stay filtered out.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"not_c1": {Kind: "filter", Input: "c",
			Predicate: protocol.Not(protocol.Eq(protocol.Col("c", "referrer_id"), protocol.Lit("c1")))},
		"out": {Kind: "project", Input: "not_c1",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("c", "id")}}},
	}, "out", "many")).Equals(`[{"id":"c4"}]`)
}

func TestPredIsNullMatchesOnlyNulls(t *testing.T) {
	d := shop(t)
	// Orders with no discount: o1, o3, o4, o6, o7.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"no_discount": {Kind: "filter", Input: "o",
			Predicate: protocol.IsNull(protocol.Col("o", "discount"))},
		"by_id": {Kind: "order", Input: "no_discount",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o1"},{"id":"o3"},{"id":"o4"},{"id":"o6"},{"id":"o7"}]`)
}

func TestPredIsNotNullMatchesOnlyPresent(t *testing.T) {
	d := shop(t)
	// Orders with a discount: o2 (10.0), o5 (25.0).
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"discounted": {Kind: "filter", Input: "o",
			Predicate: protocol.IsNotNull(protocol.Col("o", "discount"))},
		"by_id": {Kind: "order", Input: "discounted",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o2"},{"id":"o5"}]`)
}

func TestPredDeMorganAgreesOverNulls(t *testing.T) {
	d := shop(t)
	// a = (referrer_id = 'c1'), b = (referrer_id = 'c2').
	// Every customer either has a referrer in {c1, c2} (predicate FALSE) or a
	// NULL referrer (UNKNOWN) — so NOT(a OR b) keeps nothing, and De Morgan's
	// equivalent AND(NOT a, NOT b) must agree exactly.
	a := func() *protocol.Expr {
		return protocol.Eq(protocol.Col("c", "referrer_id"), protocol.Lit("c1"))
	}
	b := func() *protocol.Expr {
		return protocol.Eq(protocol.Col("c", "referrer_id"), protocol.Lit("c2"))
	}

	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"neither": {Kind: "filter", Input: "c",
			Predicate: protocol.Not(protocol.Or(a(), b()))},
	}, "neither", "many")).Empty()

	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"neither": {Kind: "filter", Input: "c",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Not(a()),
				protocol.Not(b()),
			})},
	}, "neither", "many")).Empty()
}

// ── type boundaries: coercion and casts ─────────────────────────────────────

func TestPredFloatLiteralAgainstIntColumnRejected(t *testing.T) {
	d := shop(t)
	// FINDING: a fractional literal against an int64 column is rejected at
	// bind time (422) — cross-type comparison requires an explicit cast.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"frac": {Kind: "filter", Input: "p",
			Predicate: protocol.Gt(protocol.Col("p", "stock"), protocol.Lit(1.5))},
	}, "frac", "many")).ExpectStatus(422).ExpectError("expected an int64 value")
}

func TestPredCastIntColumnToFloatComparesAgainstFraction(t *testing.T) {
	d := shop(t)
	// The escape hatch for the previous test: cast(stock to float64) > 1.5
	// keeps products with stock >= 2 — p1(12), p3(5), p4(2), p6(40), p7(100).
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"stocked": {Kind: "filter", Input: "p",
			Predicate: protocol.Gt(
				&protocol.Expr{Kind: "cast", Expr: protocol.Col("p", "stock"), To: "float64"},
				protocol.Lit(1.5))},
		"by_id": {Kind: "order", Input: "stocked",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("p", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p1"},{"id":"p3"},{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

func TestPredCastFloatToIntTruncates(t *testing.T) {
	d := shop(t)
	// cast(price to int64) over category 'paper': p7 price 5.0 → 5.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"paper": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "category"), protocol.Lit("paper"))},
		"out": {Kind: "project", Input: "paper", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("p", "id")},
			{As: "price_int", Expr: protocol.Expr{Kind: "cast",
				Expr: protocol.Col("p", "price"), To: "int64"}},
		}},
	}, "out", "many")).Equals(`[{"id":"p7","price_int":5}]`)
}

// ── arithmetic ──────────────────────────────────────────────────────────────

func TestPredArithmeticPropagatesNull(t *testing.T) {
	d := shop(t)
	// discount + 5 over o1 (NULL discount) and o2 (10.0): NULL in, NULL out.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"pair": {Kind: "filter", Input: "o",
			Predicate: protocol.Or(
				protocol.Eq(protocol.Col("o", "id"), protocol.Lit("o1")),
				protocol.Eq(protocol.Col("o", "id"), protocol.Lit("o2")))},
		"by_id": {Kind: "order", Input: "pair",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_id", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("o", "id")},
			{As: "bumped", Expr: protocol.Expr{Kind: "binary", Op: "add",
				Left: protocol.Col("o", "discount"), Right: protocol.Lit(5)}},
		}},
	}, "out", "many")).Equals(`[{"id":"o1","bumped":null},{"id":"o2","bumped":15}]`)
}

func TestPredDivisionByZeroErrors(t *testing.T) {
	d := shop(t)
	// p2 has stock 0; price / stock must fail loudly, not yield NULL or Inf.
	// FINDING: runtime evaluation errors surface as HTTP 422.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"mouse": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "id"), protocol.Lit("p2"))},
		"out": {Kind: "project", Input: "mouse", Fields: []protocol.Field{
			{As: "ratio", Expr: protocol.Expr{Kind: "binary", Op: "div",
				Left: protocol.Col("p", "price"), Right: protocol.Col("p", "stock")}},
		}},
	}, "out", "many")).ExpectStatus(422).ExpectError("division by zero")
}

func TestPredNestedArithmetic(t *testing.T) {
	d := shop(t)
	// (price - 10) * 2 for p2 (price 40.0) → 60.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"mouse": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "id"), protocol.Lit("p2"))},
		"out": {Kind: "project", Input: "mouse", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("p", "id")},
			{As: "adjusted", Expr: protocol.Expr{Kind: "binary", Op: "mul",
				Left: &protocol.Expr{Kind: "binary", Op: "sub",
					Left: protocol.Col("p", "price"), Right: protocol.Lit(10)},
				Right: protocol.Lit(2)}},
		}},
	}, "out", "many")).Equals(`[{"id":"p2","adjusted":60}]`)
}

func TestPredNegate(t *testing.T) {
	d := shop(t)
	// -price for p7 (price 5.0) → -5.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"notebook": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "id"), protocol.Lit("p7"))},
		"out": {Kind: "project", Input: "notebook", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("p", "id")},
			{As: "neg_price", Expr: protocol.Expr{Kind: "unary", Op: "negate",
				Expr: protocol.Col("p", "price")}},
		}},
	}, "out", "many")).Equals(`[{"id":"p7","neg_price":-5}]`)
}

// ── comparisons over other scalar types ─────────────────────────────────────

func TestPredBoolColumnEqualsLiteral(t *testing.T) {
	d := shop(t)
	// discontinued = false → everything except p5.
	d.Query(q(map[string]protocol.Node{
		"p": {Kind: "scan", Table: "products", Scope: "p"},
		"alive": {Kind: "filter", Input: "p",
			Predicate: protocol.Eq(protocol.Col("p", "discontinued"), protocol.Lit(false))},
		"by_id": {Kind: "order", Input: "alive",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("p", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("p", "id")}}},
	}, "out", "many")).Equals(`[{"id":"p1"},{"id":"p2"},{"id":"p3"},{"id":"p4"},{"id":"p6"},{"id":"p7"}]`)
}

func TestPredTextRange(t *testing.T) {
	d := shop(t)
	// 'B' <= name < 'D' over customers: Bob (c2), Cyn (c3).
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"mid": {Kind: "filter", Input: "c",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Gte(protocol.Col("c", "name"), protocol.Lit("B")),
				protocol.Lt(protocol.Col("c", "name"), protocol.Lit("D")),
			})},
		"by_id": {Kind: "order", Input: "mid",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("c", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "name", Expr: *protocol.Col("c", "name")}}},
	}, "out", "many")).Equals(`[{"name":"Bob"},{"name":"Cyn"}]`)
}

func TestPredOrOverNullableColumn(t *testing.T) {
	d := shop(t)
	// discount = 10 OR discount = 25: NULL discounts make both sides UNKNOWN
	// (UNKNOWN ∨ UNKNOWN = UNKNOWN, filtered) — only o2 and o5 match.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"known": {Kind: "filter", Input: "o",
			Predicate: protocol.Or(
				protocol.Eq(protocol.Col("o", "discount"), protocol.Lit(10)),
				protocol.Eq(protocol.Col("o", "discount"), protocol.Lit(25)))},
		"by_id": {Kind: "order", Input: "known",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o2"},{"id":"o5"}]`)
}

// ── predicate type errors ───────────────────────────────────────────────────

func TestPredNotOverNonBoolRejected(t *testing.T) {
	d := shop(t)
	// not(name) — negating a text column is a bind-time type error (422),
	// not a coercion.
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"bad": {Kind: "filter", Input: "c",
			Predicate: protocol.Not(protocol.Col("c", "name"))},
	}, "bad", "many")).ExpectStatus(422).ExpectError("not needs a boolean")
}
