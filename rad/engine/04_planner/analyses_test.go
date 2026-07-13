package planner_test

// Analysis tests: constraint extraction (the input to access-path selection)
// and correlation classification (the input to deduplicated correlated
// execution). Both are pure functions over bound IR; queries bind through
// the real binder so the shapes are honest.

import (
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

// extract binds a many-query over the given root and runs extraction on it.
func extract(t *testing.T, root lir.Relation) *planner.ScanConstraints {
	t.Helper()
	q := bind(t, lir.Query{Card: lir.CardMany, Root: root})
	cs := planner.ExtractConstraints(q.Root)
	if cs == nil {
		t.Fatal("no scan constraints extracted")
	}
	return cs
}

func TestExtractEquality(t *testing.T) {
	cs := extract(t, bfilter(bscan("tasks", "t"), beq(bcol("t", "status"), blit("open"))))
	d, ok := cs.Cols["status"]
	if !ok || d.Eq == nil || d.Eq.Lit == nil || !d.Eq.Lit.Equal(lir.Text("open")) {
		t.Fatalf("status domain = %+v", d)
	}
	if cs.Scan.Table.Name != "tasks" {
		t.Fatalf("scan = %s", cs.Scan.Table.Name)
	}
}

func TestExtractRanges(t *testing.T) {
	cs := extract(t, bfilter(bscan("tasks", "t"),
		band(lir.Binary{Op: lir.OpGte, L: bcol("t", "priority"), R: blit(3)},
			lir.Binary{Op: lir.OpLt, L: bcol("t", "priority"), R: blit(8)})))
	d := cs.Cols["priority"]
	if d.Lo == nil || !d.Lo.Inclusive || !d.Lo.V.Equal(lir.Int64(3)) {
		t.Fatalf("lo = %+v", d.Lo)
	}
	if d.Hi == nil || d.Hi.Inclusive || !d.Hi.V.Equal(lir.Int64(8)) {
		t.Fatalf("hi = %+v", d.Hi)
	}

	// A flipped comparison means the same range: 3 < priority ⇒ priority > 3.
	cs = extract(t, bfilter(bscan("tasks", "t"),
		lir.Binary{Op: lir.OpLt, L: blit(3), R: bcol("t", "priority")}))
	d = cs.Cols["priority"]
	if d.Lo == nil || d.Lo.Inclusive || !d.Lo.V.Equal(lir.Int64(3)) {
		t.Fatalf("flipped lo = %+v", d.Lo)
	}

	// Overlapping bounds intersect to the tightest.
	cs = extract(t, bfilter(bscan("tasks", "t"),
		band(lir.Binary{Op: lir.OpGt, L: bcol("t", "priority"), R: blit(3)},
			lir.Binary{Op: lir.OpGte, L: bcol("t", "priority"), R: blit(5)})))
	d = cs.Cols["priority"]
	if d.Lo == nil || !d.Lo.Inclusive || !d.Lo.V.Equal(lir.Int64(5)) {
		t.Fatalf("intersected lo = %+v", d.Lo)
	}
}

func TestExtractEqualityBeatsRange(t *testing.T) {
	cs := extract(t, bfilter(bscan("tasks", "t"),
		band(lir.Binary{Op: lir.OpGte, L: bcol("t", "priority"), R: blit(1)},
			beq(bcol("t", "priority"), blit(4)))))
	d := cs.Cols["priority"]
	if d.Eq == nil || d.Lo != nil {
		t.Fatalf("domain = %+v, want equality only", d)
	}
}

func TestExtractConflictingEqualitiesPoison(t *testing.T) {
	cs := extract(t, bfilter(bscan("tasks", "t"),
		band(beq(bcol("t", "status"), blit("a")), beq(bcol("t", "status"), blit("b")))))
	if _, ok := cs.Cols["status"]; ok {
		t.Fatalf("conflicting equalities kept a domain: %+v", cs.Cols["status"])
	}
}

func TestExtractContributesNothing(t *testing.T) {
	// OR trees, ne, is_null, and NULL literals never drive access paths.
	cs := extract(t, bfilter(bscan("tasks", "t"),
		lir.Binary{Op: lir.OpOr,
			L: beq(bcol("t", "status"), blit("a")),
			R: beq(bcol("t", "status"), blit("b"))}))
	if len(cs.Cols) != 0 {
		t.Fatalf("or contributed: %+v", cs.Cols)
	}
	cs = extract(t, bfilter(bscan("tasks", "t"),
		band(lir.Binary{Op: lir.OpNe, L: bcol("t", "status"), R: blit("x")},
			lir.Unary{Op: lir.OpIsNull, X: bcol("t", "assignee_id")})))
	if len(cs.Cols) != 0 {
		t.Fatalf("ne/is_null contributed: %+v", cs.Cols)
	}
	cs = extract(t, bfilter(bscan("tasks", "t"), beq(bcol("t", "assignee_id"), blit(nil))))
	if len(cs.Cols) != 0 {
		t.Fatalf("eq NULL contributed: %+v", cs.Cols)
	}
}

func TestExtractStackedFilters(t *testing.T) {
	cs := extract(t, lir.Filter{
		Input: bfilter(bscan("tasks", "t"), beq(bcol("t", "status"), blit("open"))),
		Pred:  lir.Binary{Op: lir.OpGte, L: bcol("t", "priority"), R: blit(3)},
	})
	if cs.Cols["status"].Eq == nil || cs.Cols["priority"].Lo == nil {
		t.Fatalf("stacked filters not merged: %+v", cs.Cols)
	}
}

// Outer-slot equalities extract as parameterised constraints — the physical
// planner turns them into key prefixes bound per distinct outer key.
func TestExtractOuterEquality(t *testing.T) {
	q := bind(t, forcingQuery())
	root := q.Root.(*bound.Project)
	owner := root.Fields[3].Expr.(bound.First) // spread b (3 cols), then owner
	cs := planner.ExtractConstraints(owner.Rel)
	if cs == nil {
		t.Fatal("no constraints from the owner chain")
	}
	d := cs.Cols["id"]
	if d.Eq == nil || d.Eq.Outer == nil {
		t.Fatalf("owner id domain = %+v, want an outer-slot equality", d)
	}
	if *d.Eq.Outer != 2 { // boards.owner_id holds slot 2 in the golden
		t.Fatalf("outer slot = %d, want 2", *d.Eq.Outer)
	}
}

// ── correlation classification ──────────────────────────────────────────────

func TestClassifyForcingQuery(t *testing.T) {
	q := bind(t, forcingQuery())
	root := q.Root.(*bound.Project)
	owner := root.Fields[3].Expr.(bound.First)
	tasks := root.Fields[4].Expr.(bound.Array)

	oc := planner.Classify(owner.Rel)
	if oc.Kind != planner.KeyCorrelated || len(oc.Keys) != 1 ||
		oc.Keys[0].InnerCol != "id" || oc.Keys[0].OuterSlot != 2 {
		t.Fatalf("owner correlation = %+v", oc)
	}

	// The tasks crossing shapes through a Project and pages through
	// Slice/Order — still key-correlated on board_id, with the extra
	// status literal staying an ordinary constraint.
	tc := planner.Classify(tasks.Rel)
	if tc.Kind != planner.KeyCorrelated || len(tc.Keys) != 1 ||
		tc.Keys[0].InnerCol != "board_id" || tc.Keys[0].OuterSlot != 0 {
		t.Fatalf("tasks correlation = %+v", tc)
	}

	// Inside the tasks projection: the comment count folds through an
	// Aggregate and keys on the task id.
	inner := tasks.Rel.(*bound.Project)
	count := inner.Fields[8].Expr.(bound.Scalar) // spread t (7 cols), assignee, then count
	cc := planner.Classify(count.Rel)
	if cc.Kind != planner.KeyCorrelated || len(cc.Keys) != 1 ||
		cc.Keys[0].InnerCol != "task_id" {
		t.Fatalf("comment-count correlation = %+v", cc)
	}

	// And an uncorrelated relation classifies as such.
	if got := planner.Classify(q.Root); got.Kind != planner.Uncorrelated {
		t.Fatalf("root correlation = %+v", got)
	}
}

func TestClassifyGeneral(t *testing.T) {
	// An outer slot in a non-equality: general.
	q := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "later", Expr: lir.Array{
			Rel: lir.Order{Input: bfilter(bscan("tasks", "t"),
				lir.Binary{Op: lir.OpGt, L: bcol("t", "title"), R: bcol("b", "name")}),
				Terms: []lir.OrderTerm{{Expr: bcol("t", "id")}}},
		}}},
	}})
	arr := q.Root.(*bound.Project).Fields[3].Expr.(bound.Array)
	if got := planner.Classify(arr.Rel); got.Kind != planner.GeneralCorrelated {
		t.Fatalf("range against outer = %v, want general", got.Kind)
	}

	// A key equation plus another use of the outer scope: general.
	q = bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "odd", Expr: lir.Array{
			Rel: lir.Order{Input: bfilter(bscan("tasks", "t"),
				band(beq(bcol("t", "board_id"), bcol("b", "id")),
					lir.Binary{Op: lir.OpGt, L: bcol("t", "title"), R: bcol("b", "name")})),
				Terms: []lir.OrderTerm{{Expr: bcol("t", "id")}}},
		}}},
	}})
	arr = q.Root.(*bound.Project).Fields[3].Expr.(bound.Array)
	if got := planner.Classify(arr.Rel); got.Kind != planner.GeneralCorrelated {
		t.Fatalf("key + extra outer use = %v, want general", got.Kind)
	}
}
