package planner

// The unary distinct operator: deduplicate complete rows by canonical full-row
// identity (NULL == NULL, type- and order-significant), with no ordering
// promise. Each case runs through the real client→server→engine path; results
// are pinned with an explicit order above the distinct.

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// TestDistinctCollapsesDuplicates: repeated tier values collapse to one each.
func TestDistinctCollapsesDuplicates(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"proj": lirwire.Project("c", "p", nil, []lirwire.Field{{As: "tier", Expr: lirwire.Col("c", "tier")}}),
		"dist": lirwire.Distinct("proj"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "tier")}}),
	}, "ord", "many")).Equals(`[{"tier":"bronze"},{"tier":"gold"},{"tier":"silver"}]`)
}

// TestDistinctAlreadyUnique: a unique input passes through unchanged.
func TestDistinctAlreadyUnique(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"proj": lirwire.Project("c", "p", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
		"dist": lirwire.Distinct("proj"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
	}, "ord", "many")).Equals(`[{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"},{"id":"c5"}]`)
}

// TestDistinctEmpty: an empty input stays empty.
func TestDistinctEmpty(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"f":    lirwire.Filter("c", lirwire.Binary("eq", lirwire.Col("c", "id"), lirwire.Lit(lirwire.Text("nobody")))),
		"proj": lirwire.Project("f", "p", nil, []lirwire.Field{{As: "tier", Expr: lirwire.Col("c", "tier")}}),
		"dist": lirwire.Distinct("proj"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "tier")}}),
	}, "ord", "many")).Empty()
}

// TestDistinctNullEquality: NULL == NULL, so the two null referrer_ids collapse.
func TestDistinctNullEquality(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"proj": lirwire.Project("c", "p", nil, []lirwire.Field{{As: "ref", Expr: lirwire.Col("c", "referrer_id")}}),
		"dist": lirwire.Distinct("proj"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "ref")}}),
	}, "ord", "many")).Equals(`[{"ref":null},{"ref":"c1"},{"ref":"c2"}]`)
}

// TestDistinctMultiColumn: identity is the whole row — rows differing in one
// column stay distinct (furniture/false vs furniture/true).
func TestDistinctMultiColumn(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"pr": lirwire.Scan("products", "pr"),
		"proj": lirwire.Project("pr", "p", nil, []lirwire.Field{
			{As: "cat", Expr: lirwire.Col("pr", "category")},
			{As: "disc", Expr: lirwire.Col("pr", "discontinued")},
		}),
		"dist": lirwire.Distinct("proj"),
		"ord": lirwire.Order("dist", []lirwire.OrderTerm{
			{Expr: lirwire.Col("p", "cat")},
			{Expr: lirwire.Col("p", "disc")},
		}),
	}, "ord", "many")).Equals(`[
		{"cat":"furniture","disc":false},
		{"cat":"furniture","disc":true},
		{"cat":"gear","disc":false},
		{"cat":"paper","disc":false}
	]`)
}

// TestDistinctIdempotent: distinct(distinct(x)) == distinct(x).
func TestDistinctIdempotent(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"proj": lirwire.Project("c", "p", nil, []lirwire.Field{{As: "tier", Expr: lirwire.Col("c", "tier")}}),
		"d1":   lirwire.Distinct("proj"),
		"d2":   lirwire.Distinct("d1"),
		"ord":  lirwire.Order("d2", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "tier")}}),
	}, "ord", "many")).Equals(`[{"tier":"bronze"},{"tier":"gold"},{"tier":"silver"}]`)
}

// TestDistinctDownstreamAggregate: a distinct result is an ordinary relation an
// aggregate consumes — count-distinct-tiers is count over distinct.
func TestDistinctDownstreamAggregate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":    lirwire.Scan("customers", "c"),
		"proj": lirwire.Project("c", "p", nil, []lirwire.Field{{As: "tier", Expr: lirwire.Col("c", "tier")}}),
		"dist": lirwire.Distinct("proj"),
		"agg":  lirwire.Aggregate("dist", "g", nil, []lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"ord":  lirwire.Order("agg", []lirwire.OrderTerm{{Expr: lirwire.Col("g", "n")}}),
	}, "ord", "many")).Equals(`[{"n":3}]`)
}

// TestDistinctMatchesRecursiveAdmitNew: distinct and recursive admit-new share
// one canonical row identity, so over the same bag they agree — the dups and
// the two NULLs collapse identically. The recursion's anchor is the bag and its
// step yields nothing, so admit-new dedups exactly what distinct does.
func TestDistinctMatchesRecursiveAdmitNew(t *testing.T) {
	t.Parallel()
	d := shop(t)
	cols := []lirwire.RowsColumn{{Name: "v", Type: lirwire.ScalarTypeText, Nullable: ptrBool(true)}}
	bag := [][]lirwire.Cell{{mustValue("a")}, {mustValue("a")}, {mustValue("b")}, {nil}, {nil}}
	const want = `[{"v":null},{"v":"a"},{"v":"b"}]`

	d.Query(q(map[string]lirwire.Node{
		"rows": lirwire.Rows("a", cols, bag),
		"dist": lirwire.Distinct("rows"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("a", "v")}}),
	}, "ord", "many")).Equals(want)

	d.Query(qb(map[string]lirwire.Node{
		"rows":  lirwire.Rows("a", cols, bag),
		"front": lirwire.RecursiveRef("s", "p"),
		"empty": lirwire.Filter("front", lirwire.Lit(lirwire.Bool(false))),
		"step":  lirwire.Project("empty", "st", nil, []lirwire.Field{{As: "v", Expr: lirwire.Col("p", "v")}}),
		"ref":   lirwire.Ref("s", "r"),
		"ord":   lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "v")}}),
	}, map[string]lirwire.Binding{
		"s": lirwire.Recursive("rows", "step", "new"),
	}, "ord", "many")).Equals(want)
}
