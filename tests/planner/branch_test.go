package planner

// Branch expression semantics: ordered lazy branching (SQL CASE). Arm
// predicates evaluate in document order under K3 — only TRUE matches, FALSE
// and UNKNOWN fall through — the first match's result is produced, `else`
// covers the rest, and unselected result expressions are never evaluated.
// Rejections: mismatched arm result kinds, non-boolean predicates, crossings
// anywhere under a branch, and a missing else (schema-invalid). Every
// expectation is hand-derived from the fixture table in fixtures_test.go.

import (
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// -
// selection semantics
// -

func TestBranchProjectClassifiesWithNullFallThrough(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// referrer_id = 'c1' is UNKNOWN for c1/c5 (NULL referrer) — K3
	// fall-through to the else, not a match, not an error. c2/c3 match;
	// c4's referrer is c2, so the predicate is FALSE and also falls through.
	d.Query(q(map[string]lirwire.Node{
		"c":     lirwire.Scan("customers", "c"),
		"by_id": lirwire.Order("c", []lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "src", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("c", "referrer_id"), lirwire.LitOf("c1")),
						lirwire.LitOf("ada"),
					),
				},
				lirwire.LitOf("other"),
			)},
		}),
	}, "out", "many")).Equals(`[
		{"id":"c1","src":"other"},
		{"id":"c2","src":"ada"},
		{"id":"c3","src":"ada"},
		{"id":"c4","src":"other"},
		{"id":"c5","src":"other"}]`)
}

func TestBranchFirstTrueArmWinsInOrder(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// price > 50 and price > 30 both hold for the expensive products; the
	// first arm in document order must win.
	d.Query(q(map[string]lirwire.Node{
		"p":     lirwire.Scan("products", "p"),
		"by_id": lirwire.Order("p", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "band", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("gt", lirwire.Col("p", "price"), lirwire.LitOf(50.0)),
						lirwire.LitOf("expensive"),
					),
					lirwire.Arm(
						lirwire.Binary("gt", lirwire.Col("p", "price"), lirwire.LitOf(30.0)),
						lirwire.LitOf("mid"),
					),
				},
				lirwire.LitOf("cheap"),
			)},
		}),
	}, "out", "many")).Equals(`[
		{"id":"p1","band":"expensive"},
		{"id":"p2","band":"mid"},
		{"id":"p3","band":"expensive"},
		{"id":"p4","band":"expensive"},
		{"id":"p5","band":"expensive"},
		{"id":"p6","band":"mid"},
		{"id":"p7","band":"cheap"}]`)
}

func TestBranchInFilterPredicate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A bool-typed branch is an ordinary predicate: TRUE keeps the row.
	// c1/c5 have NULL referrers, so the arm predicate is UNKNOWN and the
	// else (false) drops them; c4's referrer is c2 (FALSE, dropped).
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"referred": lirwire.Filter("c", lirwire.Branch(
			[]lirwire.BranchArm{
				lirwire.Arm(
					lirwire.Binary("eq", lirwire.Col("c", "referrer_id"), lirwire.LitOf("c1")),
					lirwire.LitOf(true),
				),
			},
			lirwire.LitOf(false),
		)),
		"by_id": lirwire.Order("referred", []lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("c", "id")}}),
	}, "out", "many")).Equals(`[{"id":"c2"},{"id":"c3"}]`)
}

func TestBranchInOrderTerm(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Sort key computed by a branch: gear last, everything else first.
	d.Query(q(map[string]lirwire.Node{
		"p": lirwire.Scan("products", "p"),
		"gear_last": lirwire.Order("p", []lirwire.OrderTerm{
			{Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("p", "category"), lirwire.LitOf("gear")),
						lirwire.LitOf(1),
					),
				},
				lirwire.LitOf(0),
			)},
			{Expr: lirwire.Col("p", "id")},
		}),
		"out": lirwire.Project("gear_last", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
	}, "out", "many")).Equals(`[
		{"id":"p4"},{"id":"p5"},{"id":"p6"},{"id":"p7"},
		{"id":"p1"},{"id":"p2"},{"id":"p3"}]`)
}

func TestBranchTypedNullElse(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A typed NULL else: only o5 (discount 25.0) clears the bar; o2 (10.0)
	// is FALSE and the NULL-discount orders are UNKNOWN — all fall through
	// to the NULL else.
	d.Query(q(map[string]lirwire.Node{
		"o":     lirwire.Scan("orders", "o"),
		"by_id": lirwire.Order("o", []lirwire.OrderTerm{{Expr: lirwire.Col("o", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("o", "id")},
			{As: "big", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("gt", lirwire.Col("o", "discount"), lirwire.LitOf(15.0)),
						lirwire.Col("o", "discount"),
					),
				},
				lirwire.Lit(lirwire.Null(lirwire.ScalarTypeFloat64)),
			)},
		}),
	}, "out", "many")).Equals(`[
		{"id":"o1","big":null},
		{"id":"o2","big":null},
		{"id":"o3","big":null},
		{"id":"o4","big":null},
		{"id":"o5","big":25},
		{"id":"o6","big":null},
		{"id":"o7","big":null}]`)
}

// -
// laziness
// -

func TestBranchLazinessSkipsUnselectedElse(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 100 / stock divides by zero for p2 and p5 (stock 0) — but for exactly
	// those rows the first arm is TRUE, so the else must never be evaluated.
	// Any error here means the branch evaluated an arm it did not select.
	d.Query(q(map[string]lirwire.Node{
		"p":     lirwire.Scan("products", "p"),
		"by_id": lirwire.Order("p", []lirwire.OrderTerm{{Expr: lirwire.Col("p", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("p", "id")},
			{As: "per_unit", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("p", "stock"), lirwire.LitOf(0)),
						lirwire.LitOf(-1),
					),
				},
				lirwire.Binary("div", lirwire.LitOf(100), lirwire.Col("p", "stock")),
			)},
		}),
	}, "out", "many")).Equals(`[
		{"id":"p1","per_unit":8},
		{"id":"p2","per_unit":-1},
		{"id":"p3","per_unit":20},
		{"id":"p4","per_unit":50},
		{"id":"p5","per_unit":-1},
		{"id":"p6","per_unit":2},
		{"id":"p7","per_unit":1}]`)
}

func TestBranchLazinessSkipsLaterWhens(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The first arm is constantly TRUE, so the second arm's predicate —
	// which divides by zero if evaluated — must never run.
	d.Query(q(map[string]lirwire.Node{
		"c":     lirwire.Scan("customers", "c"),
		"by_id": lirwire.Order("c", []lirwire.OrderTerm{{Expr: lirwire.Col("c", "id")}}),
		"out": lirwire.Project("by_id", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "n", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(lirwire.LitOf(true), lirwire.LitOf(1)),
					lirwire.Arm(
						lirwire.Binary("eq",
							lirwire.Binary("div", lirwire.LitOf(1), lirwire.LitOf(0)),
							lirwire.LitOf(1)),
						lirwire.LitOf(2),
					),
				},
				lirwire.LitOf(3),
			)},
		}),
	}, "out", "many")).Equals(`[
		{"id":"c1","n":1},
		{"id":"c2","n":1},
		{"id":"c3","n":1},
		{"id":"c4","n":1},
		{"id":"c5","n":1}]`)
}

// -
// rejections
// -

func TestBranchArmResultKindMismatchRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"out": lirwire.Project("c", "", nil, []lirwire.Field{
			{As: "x", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold")),
						lirwire.LitOf(1),
					),
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("silver")),
						lirwire.LitOf("two"),
					),
				},
				lirwire.LitOf(3),
			)},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectError("branch arm 2 result is text but arm 1 result is int64")
}

func TestBranchElseKindMismatchRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"out": lirwire.Project("c", "", nil, []lirwire.Field{
			{As: "x", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold")),
						lirwire.LitOf(1),
					),
				},
				lirwire.LitOf("other"),
			)},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectError("branch else is text but arm 1 result is int64")
}

func TestBranchNonBooleanWhenRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"out": lirwire.Project("c", "", nil, []lirwire.Field{
			{As: "x", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(lirwire.Col("c", "name"), lirwire.LitOf(1)),
				},
				lirwire.LitOf(0),
			)},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectError("branch arm 1 when must be boolean")
}

func TestBranchCrossingInWhenRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":  lirwire.Scan("customers", "c"),
		"rv": lirwire.Scan("reviews", "rv"),
		"out": lirwire.Project("c", "", nil, []lirwire.Field{
			{As: "x", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(lirwire.Exists("rv"), lirwire.LitOf(1)),
				},
				lirwire.LitOf(0),
			)},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectError("branch cannot contain a crossing (exists in arm 1's when)")
}

func TestBranchCrossingInThenRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]lirwire.Node{
		"c":  lirwire.Scan("customers", "c"),
		"rv": lirwire.Scan("reviews", "rv"),
		"out": lirwire.Project("c", "", nil, []lirwire.Field{
			{As: "x", Expr: lirwire.Branch(
				[]lirwire.BranchArm{
					lirwire.Arm(
						lirwire.Binary("eq", lirwire.Col("c", "tier"), lirwire.LitOf("gold")),
						lirwire.Exists("rv"),
					),
				},
				lirwire.LitOf(false),
			)},
		}),
	}, "out", "many")).ExpectStatus(422).ExpectError("branch cannot contain a crossing (exists in arm 1's then)")
}

func TestBranchMissingElseSchemaInvalid(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// else is required on the wire: LIR has typed NULLs only, so there is no
	// implicit-NULL default for the schema to assume.
	status, body := postQuery(t, d, `{
		"nodes": {
			"c": {"kind": "scan", "table": "customers", "scope": "c"},
			"out": {"kind": "project", "input": "c", "fields": [
				{"as": "x", "expr": {"kind": "branch", "branches": [
					{"when": {"kind": "lit", "value": {"type": "bool", "value": true}}, "then": {"kind": "lit", "value": {"type": "int64", "value": "1"}}}
				]}}
			]}
		},
		"root": {"node": "out", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("missing else: status %d, want schema rejection (400/422)\n%s", status, body)
	}
	if !strings.Contains(body, "branch") {
		t.Fatalf("rejection does not name the branch expression:\n%s", body)
	}
}
