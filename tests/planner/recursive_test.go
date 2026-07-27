package planner

// Recursive bindings over the wire: a fixpoint over the shop fixture's
// self-referential referral graph (referrer_id → customers.id). c1 referred
// c2 and c3; c2 referred c4; c5 has no referrer. So c1's referral subtree is
// {c1, c2, c3, c4}, with depths c1=0, c2=1, c3=1, c4=2.
//
// The anchor pins the root customer; the step joins customers to the frontier
// on referrer_id = frontier.id; the outer ref observes the completed value.

import (
	"maps"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// eqText builds `scope.col = "lit"`.
func eqText(scope, col, lit string) lirwire.Expr {
	return lirwire.Binary("eq", lirwire.Col(scope, col), lirwire.Lit(lirwire.Text(lit)))
}

// referralStep is the shared inductive case: customers whose referrer is a
// member of the frontier, projecting the given fields under scope "s".
func referralStep(binding string, fields []lirwire.Field) map[string]lirwire.Node {
	return map[string]lirwire.Node{
		"step_scan":     lirwire.Scan("customers", "sc"),
		"step_frontier": lirwire.RecursiveRef(binding, "parent"),
		"step_join": lirwire.Join("step_scan", "step_frontier", "inner",
			lirwire.Binary("eq", lirwire.Col("sc", "referrer_id"), lirwire.Col("parent", "id"))),
		"step_proj": lirwire.Project("step_join", "s", nil, fields),
	}
}

// TestRecursiveReachability walks the whole referral subtree with union
// distinct, returning ids only.
func TestRecursiveReachability(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := map[string]lirwire.Node{
		"anchor_scan":   lirwire.Scan("customers", "ac"),
		"anchor_filter": lirwire.Filter("anchor_scan", eqText("ac", "id", "c1")),
		"anchor_proj": lirwire.Project("anchor_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
		}),
		"ref":     lirwire.Ref("tree", "r"),
		"ordered": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}
	maps.Copy(nodes, referralStep("tree", []lirwire.Field{{As: "id", Expr: lirwire.Col("sc", "id")}}))
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "new"),
	}, "ordered", "many")).Equals(`[
		{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"}
	]`)
}

// TestRecursiveDepth carries a depth column: an int64 literal in the anchor,
// parent.depth + 1 in the step, combined with union all and ordered by
// (depth, id).
func TestRecursiveDepth(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := map[string]lirwire.Node{
		"anchor_scan":   lirwire.Scan("customers", "ac"),
		"anchor_filter": lirwire.Filter("anchor_scan", eqText("ac", "id", "c1")),
		"anchor_proj": lirwire.Project("anchor_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
			{As: "depth", Expr: lirwire.Lit(lirwire.Int64(0))},
		}),
		"ref": lirwire.Ref("tree", "r"),
		"ordered": lirwire.Order("ref", []lirwire.OrderTerm{
			{Expr: lirwire.Col("r", "depth")},
			{Expr: lirwire.Col("r", "id")},
		}),
	}
	maps.Copy(nodes, referralStep("tree", []lirwire.Field{
		{As: "id", Expr: lirwire.Col("sc", "id")},
		{As: "depth", Expr: lirwire.Binary("add", lirwire.Col("parent", "depth"), lirwire.Lit(lirwire.Int64(1)))},
	}))
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).Equals(`[
		{"id":"c1","depth":0},{"id":"c2","depth":1},{"id":"c3","depth":1},{"id":"c4","depth":2}
	]`)
}

// TestRecursiveTerminationCap: a step whose join reproduces its own frontier
// row every round (child.id = frontier.id) never terminates under union all;
// the engine's iteration cap fails it rather than hanging.
func TestRecursiveTerminationCap(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"anchor_scan":   lirwire.Scan("customers", "ac"),
		"anchor_filter": lirwire.Filter("anchor_scan", eqText("ac", "id", "c1")),
		"anchor_proj": lirwire.Project("anchor_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
		}),
		"step_scan":     lirwire.Scan("customers", "sc"),
		"step_frontier": lirwire.RecursiveRef("loop", "parent"),
		"step_join": lirwire.Join("step_scan", "step_frontier", "inner",
			lirwire.Binary("eq", lirwire.Col("sc", "id"), lirwire.Col("parent", "id"))),
		"step_proj": lirwire.Project("step_join", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("sc", "id")},
		}),
		"ref":     lirwire.Ref("loop", "r"),
		"ordered": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"loop": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectCode("execution_failed")
}

// -
// well-formedness rejections
// -

// anchorProj is a minimal valid anchor producing {id} for the root customer.
func anchorProj() map[string]lirwire.Node {
	return map[string]lirwire.Node{
		"anchor_scan":   lirwire.Scan("customers", "ac"),
		"anchor_filter": lirwire.Filter("anchor_scan", eqText("ac", "id", "c1")),
		"anchor_proj": lirwire.Project("anchor_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
		}),
	}
}

func TestRecursiveRejectsNoRecursiveRef(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := anchorProj()
	nodes["step_scan"] = lirwire.Scan("customers", "sc")
	nodes["step_proj"] = lirwire.Project("step_scan", "s", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("sc", "id")}})
	nodes["ref"] = lirwire.Ref("tree", "r")
	nodes["ordered"] = lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}})
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectStatus(422).ExpectError("no recursive_ref")
}

func TestRecursiveRejectsRefInAnchor(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := map[string]lirwire.Node{
		"anchor_frontier": lirwire.RecursiveRef("tree", "ap"),
		"anchor_proj":     lirwire.Project("anchor_frontier", "a", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("ap", "id")}}),
		"ref":             lirwire.Ref("tree", "r"),
		"ordered":         lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}
	maps.Copy(nodes, referralStep("tree", []lirwire.Field{{As: "id", Expr: lirwire.Col("sc", "id")}}))
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectStatus(422).ExpectError("anchor contains a recursive_ref")
}

func TestRecursiveRejectsTwoRefs(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := anchorProj()
	nodes["f1"] = lirwire.RecursiveRef("tree", "p1")
	nodes["f2"] = lirwire.RecursiveRef("tree", "p2")
	nodes["step_join"] = lirwire.Join("f1", "f2", "inner", lirwire.Binary("eq", lirwire.Col("p1", "id"), lirwire.Col("p2", "id")))
	nodes["step_proj"] = lirwire.Project("step_join", "s", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("p1", "id")}})
	nodes["ref"] = lirwire.Ref("tree", "r")
	nodes["ordered"] = lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}})
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectStatus(422).ExpectError("exactly one")
}

func TestRecursiveRejectsRefUnderAggregate(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := anchorProj()
	nodes["step_frontier"] = lirwire.RecursiveRef("tree", "parent")
	nodes["step_agg"] = lirwire.Aggregate("step_frontier", "g",
		[]lirwire.GroupTerm{{Expr: lirwire.Col("parent", "id")}}, nil)
	nodes["step_proj"] = lirwire.Project("step_agg", "s", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("g", "id")}})
	nodes["ref"] = lirwire.Ref("tree", "r")
	nodes["ordered"] = lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}})
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectStatus(422).ExpectError("non-monotone")
}

func TestRecursiveRejectsMutualRecursion(t *testing.T) {
	t.Parallel()
	d := shop(t)
	nodes := anchorProj()
	// The step's recursive_ref names a different binding.
	nodes["step_frontier"] = lirwire.RecursiveRef("other", "parent")
	nodes["step_join"] = lirwire.Join("step_scan", "step_frontier", "inner",
		lirwire.Binary("eq", lirwire.Col("sc", "referrer_id"), lirwire.Col("parent", "id")))
	nodes["step_scan"] = lirwire.Scan("customers", "sc")
	nodes["step_proj"] = lirwire.Project("step_join", "s", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("sc", "id")}})
	nodes["ref"] = lirwire.Ref("tree", "r")
	nodes["ordered"] = lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}})
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectStatus(422).ExpectError("mutual recursion")
}

func TestRecursiveRejectsRefOutsideStep(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A recursive_ref reached from the root, in no recursive binding's step.
	nodes := map[string]lirwire.Node{
		"frontier": lirwire.RecursiveRef("tree", "p"),
		"proj":     lirwire.Project("frontier", "x", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("p", "id")}}),
		"ordered":  lirwire.Order("proj", []lirwire.OrderTerm{{Expr: lirwire.Col("x", "id")}}),
	}
	d.Query(qb(nodes, nil, "ordered", "many")).ExpectStatus(422).ExpectError("only inside")
}

func TestRecursiveRejectsKindMismatch(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// The anchor types n as int64; the step produces text — the kinds must
	// match (nullability may widen, kinds may not).
	nodes := map[string]lirwire.Node{
		"anchor_scan":   lirwire.Scan("customers", "ac"),
		"anchor_filter": lirwire.Filter("anchor_scan", eqText("ac", "id", "c1")),
		"anchor_proj": lirwire.Project("anchor_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
			{As: "n", Expr: lirwire.Lit(lirwire.Int64(0))},
		}),
		"ref":     lirwire.Ref("tree", "r"),
		"ordered": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}
	maps.Copy(nodes, referralStep("tree", []lirwire.Field{
		{As: "id", Expr: lirwire.Col("sc", "id")},
		{As: "n", Expr: lirwire.Lit(lirwire.Text("x"))},
	}))
	d.Query(qb(nodes, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("anchor_proj", "step_proj", "all"),
	}, "ordered", "many")).ExpectStatus(422).ExpectError("kinds must match")
}
