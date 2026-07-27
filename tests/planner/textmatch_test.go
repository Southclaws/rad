package planner

// text_match semantics end to end: an anchored glob over text, boolean under
// K3. Prefix / suffix / infix / exact / multi-gap / all-wildcard shapes,
// byte-exact and Unicode-simple-fold literals, and NULL value → UNKNOWN
// dropping a row from a filter. Rejection: a non-text value. Expectations are
// hand-derived from the shop fixture in fixtures_test.go.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// idsWhere filters `table` by a text_match over `column` and returns the ids
// in order — the shared shape for every match test below.
func idsWhere(table, scope, column string, parts ...lirwire.TextMatchExprPart) lirwire.Query {
	return idsWhereCompared(table, scope, column, lirwire.TextComparisonExact, parts...)
}

func idsWhereCompared(table, scope, column string, comparison lirwire.TextComparison, parts ...lirwire.TextMatchExprPart) lirwire.Query {
	return q(map[string]lirwire.Node{
		"t": lirwire.Scan(table, scope),
		"f": lirwire.Filter("t", lirwire.TextMatchWithComparison(lirwire.Col(scope, column), comparison, parts...)),
		"o": lirwire.Order("f", []lirwire.OrderTerm{{Expr: lirwire.Col(scope, "id")}}),
		"out": lirwire.Project("o", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col(scope, "id")},
		}),
	}, "out", "many")
}

func lit(s string) lirwire.TextMatchExprPart { return lirwire.LiteralPart(s) }
func anyMany() lirwire.TextMatchExprPart     { return lirwire.AnyManyPart() }

func TestTextMatchPrefix(t *testing.T) {
	t.Parallel()
	// email LIKE 'a%' — only ada@shop.io.
	shop(t).Query(idsWhere("customers", "c", "email", lit("a"), anyMany())).
		Equals(`[{"id":"c1"}]`)
}

func TestTextMatchSuffix(t *testing.T) {
	t.Parallel()
	// name LIKE '%b' — only Bob.
	shop(t).Query(idsWhere("customers", "c", "name", anyMany(), lit("b"))).
		Equals(`[{"id":"c2"}]`)
}

func TestTextMatchInfix(t *testing.T) {
	t.Parallel()
	// name LIKE '%o%' — Keyboard, Mouse, Monitor, Notebook.
	shop(t).Query(idsWhere("products", "p", "name", anyMany(), lit("o"), anyMany())).
		Equals(`[{"id":"p1"},{"id":"p2"},{"id":"p3"},{"id":"p7"}]`)
}

func TestTextMatchExact(t *testing.T) {
	t.Parallel()
	// tier LIKE 'gold' (no wildcard) — exact, matches the gold customers.
	shop(t).Query(idsWhere("customers", "c", "tier", lit("gold"))).
		Equals(`[{"id":"c1"},{"id":"c3"}]`)
}

func TestTextMatchMultiGap(t *testing.T) {
	t.Parallel()
	// name LIKE 'M%r' — starts M, ends r: Monitor only (Mouse ends in e).
	shop(t).Query(idsWhere("products", "p", "name", lit("M"), anyMany(), lit("r"))).
		Equals(`[{"id":"p3"}]`)
}

func TestTextMatchAllWildcard(t *testing.T) {
	t.Parallel()
	// name LIKE '%' — matches every non-NULL value.
	shop(t).Query(idsWhere("customers", "c", "name", anyMany())).
		Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"},{"id":"c5"}]`)
}

func TestTextMatchByteExactCase(t *testing.T) {
	t.Parallel()
	// Exact is the default: the lowercase-stored 'gold' tiers never match the
	// uppercase literal, so every row projects false.
	shop(t).Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"o": lirwire.Order("c", []lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("o", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "m", Expr: lirwire.TextMatch(lirwire.Col("c", "tier"), lit("GOLD"))},
		}),
	}, "out", "many")).Equals(`[
		{"id":"c1","m":false},{"id":"c2","m":false},{"id":"c3","m":false},
		{"id":"c4","m":false},{"id":"c5","m":false}]`)
}

func TestTextMatchUnicodeSimpleFoldCase(t *testing.T) {
	t.Parallel()
	// The uppercase literal matches the lowercase stored tiers only when the
	// LIR explicitly asks for locale-independent Unicode simple folding.
	shop(t).Query(idsWhereCompared(
		"customers", "c", "tier", lirwire.TextComparisonUnicodeSimpleFold, lit("GOLD"),
	)).Equals(`[{"id":"c1"},{"id":"c3"}]`)
}

func TestTextMatchNullValueDropsRow(t *testing.T) {
	t.Parallel()
	// referrer_id LIKE 'c1': c2/c3 match; c4's referrer is c2 (FALSE); c1/c5
	// have a NULL referrer, so the predicate is UNKNOWN and the filter drops
	// them — never an error.
	shop(t).Query(idsWhere("customers", "c", "referrer_id", lit("c1"))).
		Equals(`[{"id":"c2"},{"id":"c3"}]`)
}

func TestTextMatchRejectsNonTextValue(t *testing.T) {
	t.Parallel()
	// created_at is int64; text_match's value must be text.
	shop(t).Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"f": lirwire.Filter("c", lirwire.TextMatch(lirwire.Col("c", "created_at"), lit("x"))),
		"o": lirwire.Order("f", []lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("o", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectError("must be text")
}
