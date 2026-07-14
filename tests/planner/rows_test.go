package planner

// The rows node: finite constant relations. Column types are declared, cell
// coercion follows the literal rules, nullability derives from the cells,
// and the relation is an ordinary bag — joinable, filterable, aggregatable,
// bindable. Every query is the literal wire tree.

import (
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

// cols is shorthand for declaring rows columns; a trailing "?" on the type
// marks the column nullable ("int64?").
func cols(pairs ...string) []protocol.RowsColumn {
	out := make([]protocol.RowsColumn, 0, len(pairs)/2)
	for i := 0; i < len(pairs); i += 2 {
		typ, nullable := strings.CutSuffix(pairs[i+1], "?")
		out = append(out, protocol.RowsColumn{Name: pairs[i], Type: typ, Nullable: nullable})
	}
	return out
}

func TestRowsBasic(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("name", "text", "age", "int64"),
			Rows:    [][]any{{"ada", 36}, {"grace", 41}}},
	}, "r", "many")).EqualsUnordered(`[
		{"name":"ada","age":36},
		{"name":"grace","age":41}
	]`)
}

// Document order of literal rows is not a logical order; an explicit order
// above the node is what makes the sequence observable.
func TestRowsOrdered(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{3}, {1}, {2}}},
		"o": {Kind: "order", Input: "r",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("r", "n")}}},
	}, "o", "many")).Equals(`[{"n":1},{"n":2},{"n":3}]`)
}

// With declared types, the empty constant relation is well-typed: zero rows,
// fully usable output schema.
func TestRowsEmpty(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("id", "int64", "label", "text"),
			Rows:    [][]any{}},
		"out": {Kind: "project", Input: "r",
			Fields: []protocol.Field{{As: "l", Expr: *protocol.Col("r", "label")}}},
	}, "out", "many")).Empty()
}

// All four scalar types plus NULL cells in declared-nullable columns;
// is_null is the only test that matches one.
func TestRowsAllTypesAndNulls(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("t", "text?", "i", "int64?", "f", "float64?", "b", "bool?"),
			Rows: [][]any{
				{"x", 1, 1.5, true},
				{nil, nil, nil, nil},
			}},
		"nulls": {Kind: "filter", Input: "r",
			Predicate: protocol.IsNull(protocol.Col("r", "t"))},
	}, "nulls", "many")).Equals(`[{"t":null,"i":null,"f":null,"b":null}]`)
}

// Nullability is declared, never inferred: a NULL cell in a non-nullable
// column is rejected, and a nullable column with no nulls in this
// particular bag is fine — the schema does not depend on the data.
func TestRowsNullabilityIsDeclared(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{1}, {nil}}},
	}, "r", "many")).ExpectStatus(422).ExpectError("not nullable")

	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64?"),
			Rows:    [][]any{{1}, {2}}},
	}, "r", "many")).EqualsUnordered(`[{"n":1},{"n":2}]`)
}

// Raw JSON numbers keep full int64 precision — a value beyond float64's
// exact-integer range survives the round trip bit-for-bit.
func TestRowsInt64Precision(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("big", "int64"),
			Rows:    [][]any{{int64(9007199254740993)}}},
	}, "r", "many")).Equals(`[{"big":9007199254740993}]`)
}

// A one-row constant relation is statically exactly one row, so first needs
// no order; a multi-row one without an order is rejected — taking "the
// first" of an unordered bag would be arbitrary.
func TestRowsCardinalityUnderFirst(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{7}}},
	}, "r", "first")).EqualsDatum(`{"n":7}`)

	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{1}, {2}}},
	}, "r", "first")).ExpectError("unordered")
}

// scalar over a single-column, single-row constant relation.
func TestRowsScalarRoot(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("answer", "int64"),
			Rows:    [][]any{{42}}},
	}, "r", "scalar")).EqualsDatum(`42`)
}

// The flagship composition: join stored rows against an ad hoc mapping. The
// constant relation behaves exactly like a table on the right of a join.
func TestRowsJoinAgainstTable(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"m": {Kind: "rows", Scope: "m",
			Columns: cols("tier", "text", "rank", "int64"),
			Rows:    [][]any{{"gold", 1}, {"silver", 2}, {"bronze", 3}}},
		"j": {Kind: "join", Left: "c", Right: "m", Join: "inner",
			On: protocol.Eq(protocol.Col("c", "tier"), protocol.Col("m", "tier"))},
		"out": {Kind: "project", Input: "j", Fields: []protocol.Field{
			{As: "name", Expr: *protocol.Col("c", "name")},
			{As: "rank", Expr: *protocol.Col("m", "rank")},
		}},
	}, "out", "many")).EqualsUnordered(`[
		{"name":"Ada","rank":1},
		{"name":"Cyn","rank":1},
		{"name":"Bob","rank":2},
		{"name":"Dee","rank":3},
		{"name":"Eli","rank":3}
	]`)
}

// Constant relations fold like any other relation.
func TestRowsAggregate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("team", "text", "points", "int64"),
			Rows: [][]any{
				{"red", 3}, {"blue", 5}, {"red", 4}, {"blue", 1},
			}},
		"agg": {Kind: "aggregate", Input: "r", Scope: "g",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("r", "team")}},
			Aggs:   []protocol.AggTerm{{Fn: "sum", Arg: protocol.Col("r", "points"), As: "total"}}},
		"o": {Kind: "order", Input: "agg",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("g", "team")}}},
	}, "o", "many")).Equals(`[{"team":"blue","total":6},{"team":"red","total":7}]`)
}

// A binding whose body is a constant relation: both occurrences observe the
// same bag under fresh scopes, exactly as with any other binding.
func TestRowsAsBinding(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]protocol.Node{
		"lit": {Kind: "rows", Scope: "l",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{1}, {2}}},
		"a": {Kind: "ref", Binding: "nums", Scope: "a"},
		"b": {Kind: "ref", Binding: "nums", Scope: "b"},
		"j": {Kind: "join", Left: "a", Right: "b", Join: "inner",
			On: protocol.Lt(protocol.Col("a", "n"), protocol.Col("b", "n"))},
		"out": {Kind: "project", Input: "j", Fields: []protocol.Field{
			{As: "lo", Expr: *protocol.Col("a", "n")},
			{As: "hi", Expr: *protocol.Col("b", "n")},
		}},
	}, map[string]protocol.Binding{"nums": {Node: "lit"}},
		"out", "many")).EqualsUnordered(`[{"lo":1,"hi":2}]`)
}

// A correlated crossing whose sub-relation is a constant: exists over rows
// filtered by an outer column.
func TestRowsCorrelatedCrossing(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"vip": {Kind: "rows", Scope: "v",
			Columns: cols("tier", "text"),
			Rows:    [][]any{{"gold"}}},
		"match": {Kind: "filter", Input: "vip",
			Predicate: protocol.Eq(protocol.Col("v", "tier"), protocol.Col("c", "tier"))},
		"keep": {Kind: "filter", Input: "c",
			Predicate: protocol.Exists("match")},
		"out": {Kind: "project", Input: "keep",
			Fields: []protocol.Field{{As: "name", Expr: *protocol.Col("c", "name")}}},
	}, "out", "many")).EqualsUnordered(`[{"name":"Ada"},{"name":"Cyn"}]`)
}

// ── rejections ──────────────────────────────────────────────────────────────

func TestRowsRejectsDuplicateColumns(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64", "n", "text"),
			Rows:    [][]any{{1, "x"}}},
	}, "r", "many")).ExpectStatus(422).ExpectError("declares column")
}

func TestRowsRejectsArityMismatch(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("a", "int64", "b", "text"),
			Rows:    [][]any{{1, "x"}, {2}}},
	}, "r", "many")).ExpectStatus(422).ExpectError("want 2")
}

func TestRowsRejectsCellTypeMismatch(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{"ten"}}},
	}, "r", "many")).ExpectStatus(422).ExpectError("int64")
}

func TestRowsRejectsFractionalInt(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{1.5}}},
	}, "r", "many")).ExpectStatus(422).ExpectError("int64")
}

func TestRowsRejectsUnknownColumnType(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The schema itself rejects the type enum — client-side, before the
	// request is even sent.
	d.Query(q(map[string]protocol.Node{
		"r": {Kind: "rows", Scope: "r",
			Columns: []protocol.RowsColumn{{Name: "n", Type: "varchar"}},
			Rows:    [][]any{{1}}},
	}, "r", "many")).ExpectError("varchar")
}

func TestRowsRejectsMissingColumns(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The schema requires at least one declared column; the client-side
	// validation catches it before the request is sent. Built without the
	// q() helper: its order synthesis needs a first column to exist.
	d.Query(protocol.Query{
		Nodes: map[string]protocol.Node{
			"r": {Kind: "rows", Scope: "r", Rows: [][]any{{1}}},
		},
		Root: protocol.Root{Node: "r", Cardinality: "many"},
	}).ExpectError("minItems")
}

func TestRowsRejectsDuplicateScope(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"r": {Kind: "rows", Scope: "c",
			Columns: cols("n", "int64"),
			Rows:    [][]any{{1}}},
		"j": {Kind: "join", Left: "c", Right: "r", Join: "inner",
			On: protocol.Lit(true)},
	}, "j", "many")).ExpectStatus(422).ExpectError("duplicate scope")
}
