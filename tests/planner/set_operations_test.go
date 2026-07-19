package planner

// The set-operation family: concatenate (n-ary bag concatenation, SQL UNION
// ALL), intersect, and except (binary, each with an all/distinct
// quantifier). All share positional compatibility and expose their output
// under the node's own scope; intersect/except and distinct share canonical
// full-row identity. Each case runs through the real client→server→engine
// path; results are pinned with an explicit order above the operation.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// TestConcatenatePreservesDuplicates: a row produced by both inputs appears
// twice — concatenation never deduplicates. Ada and Cyn are gold; Ada, Bob,
// and Cyn joined by created_at 300; the overlap survives with its
// multiplicity.
func TestConcatenatePreservesDuplicates(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c1":   lirwire.Scan("customers", "c1"),
		"gold": lirwire.Filter("c1", lirwire.Binary("eq", lirwire.Col("c1", "tier"), lirwire.LitOf("gold"))),
		"p1":   lirwire.Project("gold", "s1", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c1", "name")}}),
		"c2":   lirwire.Scan("customers", "c2"),
		"early": lirwire.Filter("c2",
			lirwire.Binary("lte", lirwire.Col("c2", "created_at"), lirwire.LitOf(300))),
		"p2":  lirwire.Project("early", "s2", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c2", "name")}}),
		"u":   lirwire.Concatenate("u", "p1", "p2"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "name")}}),
	}, "ord", "many")).Equals(`[
		{"name":"Ada"},{"name":"Ada"},{"name":"Bob"},{"name":"Cyn"},{"name":"Cyn"}
	]`)
}

// TestConcatenateUnderDistinct: set union is the composition
// distinct(concatenate(...)) — the duplicated rows of the previous case
// collapse to one each.
func TestConcatenateUnderDistinct(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c1":   lirwire.Scan("customers", "c1"),
		"gold": lirwire.Filter("c1", lirwire.Binary("eq", lirwire.Col("c1", "tier"), lirwire.LitOf("gold"))),
		"p1":   lirwire.Project("gold", "s1", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c1", "name")}}),
		"c2":   lirwire.Scan("customers", "c2"),
		"early": lirwire.Filter("c2",
			lirwire.Binary("lte", lirwire.Col("c2", "created_at"), lirwire.LitOf(300))),
		"p2":   lirwire.Project("early", "s2", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c2", "name")}}),
		"u":    lirwire.Concatenate("u", "p1", "p2"),
		"dist": lirwire.Distinct("u"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "name")}}),
	}, "ord", "many")).Equals(`[{"name":"Ada"},{"name":"Bob"},{"name":"Cyn"}]`)
}

// TestConcatenateAcrossTables: inputs from two different tables concatenate
// when their projected shapes match positionally.
func TestConcatenateAcrossTables(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"pr":    lirwire.Scan("products", "pr"),
		"paper": lirwire.Filter("pr", lirwire.Binary("eq", lirwire.Col("pr", "category"), lirwire.LitOf("paper"))),
		"p1":    lirwire.Project("paper", "s1", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("pr", "name")}}),
		"c":     lirwire.Scan("customers", "c"),
		"silver": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("silver"))),
		"p2":  lirwire.Project("silver", "s2", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c", "name")}}),
		"u":   lirwire.Concatenate("u", "p1", "p2"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "name")}}),
	}, "ord", "many")).Equals(`[{"name":"Bob"},{"name":"Notebook"}]`)
}

// TestConcatenateVariadic: concatenation is n-ary, not a binary chain —
// three inputs in one node.
func TestConcatenateVariadic(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, [][]lirwire.Cell{{mustValue(3)}, {mustValue(1)}}),
		"b":   lirwire.Rows("b", cols, [][]lirwire.Cell{{mustValue(2)}}),
		"c":   lirwire.Rows("c", cols, [][]lirwire.Cell{{mustValue(1)}}),
		"u":   lirwire.Concatenate("u", "a", "b", "c"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":1},{"v":1},{"v":2},{"v":3}]`)
}

// TestConcatenateWidensNullability: a NULL from the nullable input flows
// through a column the other input declares non-nullable — the output
// column is nullable when any input's is.
func TestConcatenateWidensNullability(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c1": lirwire.Scan("customers", "c1"),
		"p1": lirwire.Project("c1", "s1", nil, []lirwire.Field{{As: "who", Expr: lirwire.Col("c1", "referrer_id")}}),
		"c2": lirwire.Scan("customers", "c2"),
		"eli": lirwire.Filter("c2",
			lirwire.Binary("eq", lirwire.Col("c2", "id"), lirwire.LitOf("c5"))),
		"p2":  lirwire.Project("eli", "s2", nil, []lirwire.Field{{As: "who", Expr: lirwire.Col("c2", "id")}}),
		"u":   lirwire.Concatenate("u", "p1", "p2"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "who")}}),
	}, "ord", "many")).Equals(`[
		{"who":null},{"who":null},{"who":"c1"},{"who":"c1"},{"who":"c2"},{"who":"c5"}
	]`)
}

// TestConcatenateDownstreamAggregate: a concatenation result is an ordinary
// relation — a fold above it counts the combined bag.
func TestConcatenateDownstreamAggregate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	d.Query(q(map[string]lirwire.Node{
		"a":    lirwire.Rows("a", cols, [][]lirwire.Cell{{mustValue(1)}, {mustValue(2)}}),
		"b":    lirwire.Rows("b", cols, [][]lirwire.Cell{{mustValue(2)}}),
		"u":    lirwire.Concatenate("u", "a", "b"),
		"fold": lirwire.Aggregate("u", "g", nil, []lirwire.AggTerm{{Fn: "count", As: "n"}}),
	}, "fold", "exactly_one")).Equals(`[{"n":3}]`)
}

// TestConcatenateInRecursiveStep: concatenation is monotone, so it is legal
// over the frontier path of a recursive step. The step unions rows derived
// from the frontier with a constant relation; admit-new accumulation
// reaches the fixpoint {1,2,3,10}.
func TestConcatenateInRecursiveStep(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	d.Query(qb(map[string]lirwire.Node{
		"seed":  lirwire.Rows("a", cols, [][]lirwire.Cell{{mustValue(1)}}),
		"front": lirwire.RecursiveRef("s", "fr"),
		"grow":  lirwire.Filter("front", lirwire.Binary("lt", lirwire.Col("fr", "v"), lirwire.LitOf(3))),
		"next": lirwire.Project("grow", "pv", nil, []lirwire.Field{
			{As: "v", Expr: lirwire.Binary("add", lirwire.Col("fr", "v"), lirwire.LitOf(1))},
		}),
		"extra": lirwire.Rows("b", cols, [][]lirwire.Cell{{mustValue(10)}}),
		"step":  lirwire.Concatenate("st", "next", "extra"),
		"ref":   lirwire.Ref("s", "r"),
		"ord":   lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "v")}}),
	}, map[string]lirwire.Binding{
		"s": lirwire.Recursive("seed", "step", "new"),
	}, "ord", "many")).Equals(`[{"v":1},{"v":2},{"v":3},{"v":10}]`)
}

// TestIntersectQuantifiers: bag intersection takes the minimum multiplicity
// under all, and one occurrence per common row under distinct.
// A = [1,1,2,2,3], B = [1,1,1,2,4].
func TestIntersectQuantifiers(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	a := [][]lirwire.Cell{{mustValue(1)}, {mustValue(1)}, {mustValue(2)}, {mustValue(2)}, {mustValue(3)}}
	b := [][]lirwire.Cell{{mustValue(1)}, {mustValue(1)}, {mustValue(1)}, {mustValue(2)}, {mustValue(4)}}

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Intersect("u", "a", "b", "all"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":1},{"v":1},{"v":2}]`)

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Intersect("u", "a", "b", "distinct"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":1},{"v":2}]`)
}

// TestExceptQuantifiers: bag difference subtracts occurrence counts under
// all (max(m−n, 0)) and removes every occurrence under distinct.
// A = [1,1,2,2,3], B = [1,1,1,2,4].
func TestExceptQuantifiers(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	a := [][]lirwire.Cell{{mustValue(1)}, {mustValue(1)}, {mustValue(2)}, {mustValue(2)}, {mustValue(3)}}
	b := [][]lirwire.Cell{{mustValue(1)}, {mustValue(1)}, {mustValue(1)}, {mustValue(2)}, {mustValue(4)}}

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Except("u", "a", "b", "all"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":2},{"v":3}]`)

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Except("u", "a", "b", "distinct"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":3}]`)
}

// TestSetOperationNullIdentity: intersect and except match rows by canonical
// full-row identity, where NULL equals NULL — unlike predicate equality.
// A = [NULL, NULL, 1], B = [NULL].
func TestSetOperationNullIdentity(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64, Nullable: ptrBool(true)}}
	a := [][]lirwire.Cell{{nil}, {nil}, {mustValue(1)}}
	b := [][]lirwire.Cell{{nil}}

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Intersect("u", "a", "b", "all"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":null}]`)

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Except("u", "a", "b", "all"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":null},{"v":1}]`)

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", cols, a),
		"b":   lirwire.Rows("b", cols, b),
		"u":   lirwire.Except("u", "a", "b", "distinct"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}}),
	}, "ord", "many")).Equals(`[{"v":1}]`)
}

// TestIntersectAcrossTables: whole-row identity across two different tables
// — customers who are gold AND joined by created_at 300, as an intersection
// of two projections of the same shape.
func TestIntersectAcrossTables(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c1":   lirwire.Scan("customers", "c1"),
		"gold": lirwire.Filter("c1", lirwire.Binary("eq", lirwire.Col("c1", "tier"), lirwire.LitOf("gold"))),
		"p1":   lirwire.Project("gold", "s1", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c1", "name")}}),
		"c2":   lirwire.Scan("customers", "c2"),
		"early": lirwire.Filter("c2",
			lirwire.Binary("lte", lirwire.Col("c2", "created_at"), lirwire.LitOf(300))),
		"p2":  lirwire.Project("early", "s2", nil, []lirwire.Field{{As: "name", Expr: lirwire.Col("c2", "name")}}),
		"u":   lirwire.Intersect("u", "p1", "p2", "distinct"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "name")}}),
	}, "ord", "many")).Equals(`[{"name":"Ada"},{"name":"Cyn"}]`)
}

// TestExceptRightSideOfRecursiveStepRejected: a growing frontier on the
// right side of an except shrinks the result — anti-monotone — so a
// recursive_ref there is rejected.
func TestExceptRightSideOfRecursiveStepRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	d.Query(qb(map[string]lirwire.Node{
		"seed":  lirwire.Rows("a", cols, [][]lirwire.Cell{{mustValue(1)}}),
		"front": lirwire.RecursiveRef("s", "fr"),
		"pool":  lirwire.Rows("b", cols, [][]lirwire.Cell{{mustValue(1)}, {mustValue(2)}}),
		"step":  lirwire.Except("st", "pool", "front", "all"),
		"ref":   lirwire.Ref("s", "r"),
		"ord":   lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "v")}}),
	}, map[string]lirwire.Binding{
		"s": lirwire.Recursive("seed", "step", "new"),
	}, "ord", "many")).ExpectError("non-monotone position")
}

// TestSetOperationRejections: the positional-compatibility contract and
// input independence, each named precisely by the binder, with the failing
// operator's own name in the message.
func TestSetOperationRejections(t *testing.T) {
	t.Parallel()
	d := shop(t)

	one := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeInt64}}
	two := []lirwire.RowsColumn{
		{Name: "v", Type: lirwire.ScalarTypeInt64},
		{Name: "w", Type: lirwire.ScalarTypeInt64},
	}
	none := [][]lirwire.Cell{}

	orderByV := lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "v")}})

	// Arity mismatch.
	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", one, none),
		"b":   lirwire.Rows("b", two, none),
		"u":   lirwire.Concatenate("u", "a", "b"),
		"ord": orderByV,
	}, "ord", "many")).ExpectStatus(422).ExpectError("concatenate inputs must have the same number of columns")

	// Name mismatch at a position.
	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", one, none),
		"b":   lirwire.Rows("b", []lirwire.RowsColumn{{Name: "x", Type: lirwire.ScalarTypeInt64}}, none),
		"u":   lirwire.Concatenate("u", "a", "b"),
		"ord": orderByV,
	}, "ord", "many")).ExpectError("concatenate matches columns positionally")

	// Kind mismatch at a position.
	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", one, none),
		"b":   lirwire.Rows("b", []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeText}}, none),
		"u":   lirwire.Concatenate("u", "a", "b"),
		"ord": orderByV,
	}, "ord", "many")).ExpectError(`concatenate column 1 ("v") is int64 in input 1 but text in input 2`)

	// The binary operations reject under their own names.
	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", one, none),
		"b":   lirwire.Rows("b", two, none),
		"u":   lirwire.Except("u", "a", "b", "all"),
		"ord": orderByV,
	}, "ord", "many")).ExpectError("except inputs must have the same number of columns")

	d.Query(q(map[string]lirwire.Node{
		"a":   lirwire.Rows("a", one, none),
		"b":   lirwire.Rows("b", []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeText}}, none),
		"u":   lirwire.Intersect("u", "a", "b", "distinct"),
		"ord": orderByV,
	}, "ord", "many")).ExpectError(`intersect column 1 ("v") is int64 in input 1 but text in input 2`)

	// An input may not reference a sibling input's scope.
	d.Query(q(map[string]lirwire.Node{
		"c1": lirwire.Scan("customers", "c1"),
		"c2": lirwire.Scan("customers", "c2"),
		"dep": lirwire.Filter("c2",
			lirwire.Binary("eq", lirwire.Col("c2", "id"), lirwire.Col("c1", "id"))),
		"u":   lirwire.Concatenate("u", "c1", "dep"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "id")}}),
	}, "ord", "many")).ExpectError("concatenate inputs are independent relations")

	d.Query(q(map[string]lirwire.Node{
		"c1": lirwire.Scan("customers", "c1"),
		"c2": lirwire.Scan("customers", "c2"),
		"dep": lirwire.Filter("c2",
			lirwire.Binary("eq", lirwire.Col("c2", "id"), lirwire.Col("c1", "id"))),
		"u":   lirwire.Intersect("u", "c1", "dep", "all"),
		"ord": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "id")}}),
	}, "ord", "many")).ExpectError("intersect inputs are independent relations")
}
