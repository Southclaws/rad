package planner_test

// Physical planning tests: access selection (v1 chooseAccess parity plus
// ranges), the limited ordered-index pushdown, and the forcing-query plan —
// three key-correlated attaches, a point get per distinct parent key, and
// the residual filter above every access node.

import (
	"strings"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

func planOf(t *testing.T, root lir.Relation) string {
	t.Helper()
	q := bind(t, lir.Query{Card: lir.CardMany, Root: root})
	return planner.PrintPlan(planner.PlanQuery(q))
}

func wantPlan(t *testing.T, got, want string) {
	t.Helper()
	if got != want {
		t.Fatalf("plan drifted.\n--- got ---\n%s\n--- want ---\n%s", got, want)
	}
}

// Complete-PK equality is a point get; the full predicate still rides above
// it — the structural residual.
func TestPlanPKLookup(t *testing.T) {
	got := planOf(t, bfilter(bscan("tasks", "t"),
		band(beq(bcol("t", "id"), blit("t1")), beq(bcol("t", "status"), blit("open")))))
	wantPlan(t, got, `Plan card=many
  Filter and(eq(t.id#0, "t1"), eq(t.status#3, "open"))
    PKGet tasks [id = "t1"]
`)
}

// The longest leading equality prefix picks the index; a non-leading
// equality cannot use it (v1 parity).
func TestPlanIndexPrefixSelection(t *testing.T) {
	got := planOf(t, bfilter(bscan("tasks", "t"),
		band(beq(bcol("t", "board_id"), blit("b1")), beq(bcol("t", "status"), blit("open")))))
	wantPlan(t, got, `Plan card=many
  Filter and(eq(t.board_id#1, "b1"), eq(t.status#3, "open"))
    IndexRangeScan tasks tasks_board_status_idx [board_id = "b1", status = "open"]
`)

	got = planOf(t, bfilter(bscan("tasks", "t"), beq(bcol("t", "status"), blit("open"))))
	wantPlan(t, got, `Plan card=many
  Filter eq(t.status#3, "open")
    TableScan tasks
`)
}

// A trailing range bound rides the index after the equality prefix —
// inequalities finally drive access paths.
func TestPlanEqPlusRange(t *testing.T) {
	got := planOf(t, bfilter(bscan("tasks", "t"),
		band(beq(bcol("t", "board_id"), blit("b1")),
			lir.Binary{Op: lir.OpGte, L: bcol("t", "status"), R: blit("m")})))
	wantPlan(t, got, `Plan card=many
  Filter and(eq(t.board_id#1, "b1"), gte(t.status#3, "m"))
    IndexRangeScan tasks tasks_board_status_idx [board_id = "b1", status >= "m"]
`)
}

// A range on an index's first column is usable with no equalities at all.
func TestPlanRangeOnly(t *testing.T) {
	got := planOf(t, bfilter(bscan("tasks", "t"),
		lir.Binary{Op: lir.OpLt, L: bcol("t", "board_id"), R: blit("m")}))
	wantPlan(t, got, `Plan card=many
  Filter lt(t.board_id#1, "m")
    IndexRangeScan tasks tasks_board_status_idx [board_id < "m"]
`)
}

// Ordered-index pushdown: when an index's provided order (columns after the
// equality prefix, then the primary key) satisfies the requirement, the
// sort disappears — the binder's id tie-breaker matches the pk suffix
// exactly. Descending never pushes down (the KV has no reverse scan), and a
// table scan already provides pk order.
func TestPlanOrderedIndexPushdown(t *testing.T) {
	got := planOf(t, lir.Order{
		Input: bscan("users", "u"),
		Terms: []lir.OrderTerm{{Expr: bcol("u", "name")}},
	})
	wantPlan(t, got, `Plan card=many
  IndexRangeScan users users_name_uq
`)

	got = planOf(t, lir.Order{
		Input: bscan("users", "u"),
		Terms: []lir.OrderTerm{{Expr: bcol("u", "name"), Desc: true}},
	})
	wantPlan(t, got, `Plan card=many
  Sort u.name#1 desc, id#0 asc
    TableScan users
`)

	got = planOf(t, lir.Order{
		Input: bscan("tasks", "t"),
		Terms: []lir.OrderTerm{{Expr: bcol("t", "id")}},
	})
	wantPlan(t, got, `Plan card=many
  TableScan tasks
`)
}

// The whole point: ordering + slicing over an equality-pinned index keeps
// the pipeline lazy end to end when the index provides the order, so the
// slice stops the scan early.
func TestPlanStopEarlyShape(t *testing.T) {
	got := planOf(t, lir.Slice{
		Input: lir.Order{
			Input: bfilter(bscan("tasks", "t"), beq(bcol("t", "board_id"), blit("b1"))),
			Terms: []lir.OrderTerm{{Expr: bcol("t", "status")}},
		},
		Limit: new(int(10)),
	})
	wantPlan(t, got, `Plan card=many
  Slice offset=0 limit=10
    Filter eq(t.board_id#1, "b1")
      IndexRangeScan tasks tasks_board_status_idx [board_id = "b1"]
`)
}

func TestPlanForcingQueryGolden(t *testing.T) {
	q := bind(t, forcingQuery())
	got := planner.PrintPlan(planner.PlanQuery(q))
	wantPlan(t, got, forcingPlanGolden)

	// The headline properties, asserted independently of formatting: every
	// crossing — owner, tasks, assignee, comment_count — attaches
	// key-correlated, and both to-parent patterns are point gets.
	if strings.Count(got, "key-correlated") != 4 {
		t.Fatalf("want 4 key-correlated attaches:\n%s", got)
	}
	if strings.Count(got, "PKGet users") != 2 {
		t.Fatalf("to-parent patterns must be point gets:\n%s", got)
	}
	// The one full scan is the root ("all boards"); every child fetch rides
	// a key.
	if strings.Count(got, "TableScan") != 1 {
		t.Fatalf("only the root may scan a whole table:\n%s", got)
	}
}

const forcingPlanGolden = `Plan card=many
  Project
    id#0 = b.id#0
    name#1 = b.name#1
    owner_id#2 = b.owner_id#2
    owner#5 = owner#5
    tasks#21 = tasks#21
    Attach
      #5 = first key-correlated [id#3 = @2]
        Filter eq(o.id#3, b.owner_id#2)
          PKGet users [id = @2]
      #21 = array key-correlated [board_id#7 = @0]
        Project
          id#6 = t.id#6
          board_id#7 = t.board_id#7
          title#8 = t.title#8
          status#9 = t.status#9
          priority#10 = t.priority#10
          estimate#11 = t.estimate#11
          assignee_id#12 = t.assignee_id#12
          assignee#15 = assignee#15
          comment_count#20 = comment_count#20
          Attach
            #15 = first key-correlated [id#13 = @12]
              Filter eq(a.id#13, t.assignee_id#12)
                PKGet users [id = @12]
            #20 = scalar key-correlated [task_id#17 = @6]
              Aggregate n#19=count(*)
                Filter eq(c.task_id#17, t.id#6)
                  IndexRangeScan comments comments_task_idx [task_id = @6]
            Slice offset=0 limit=20
              Sort t.priority#10 desc, id#6 asc
                Filter and(eq(t.board_id#7, b.id#0), eq(t.status#9, "open"))
                  IndexRangeScan tasks tasks_board_status_idx [board_id = @0, status = "open"]
      TableScan boards
`

// A binding lowers once and prints once; each occurrence is a Ref line.
// The arbitrary slice marks its binding plan-choice-sensitive; the
// deterministic filter binding is not.
func TestPlanBindings(t *testing.T) {
	q := bind(t, lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"top": lir.Slice{Input: bscan("tasks", "t"), Limit: new(int(2))},
		},
		Root: lir.Project{
			Input: lir.Join{
				Left:  lir.Ref{Binding: "top", Scope: "a"},
				Right: lir.Ref{Binding: "top", Scope: "b"},
				Kind:  lir.InnerJoin,
				On:    beq(bcol("a", "id"), bcol("b", "id")),
			},
			Scope:  "j",
			Fields: []lir.ProjField{{As: "aid", Expr: bcol("a", "id")}, {As: "bid", Expr: bcol("b", "id")}},
		},
	})
	got := planner.PrintPlan(planner.PlanQuery(q))
	wantPlan(t, got, `Plan card=many
  Binding top materialise plan-choice-sensitive
    Slice offset=0 limit=2
      TableScan tasks
  Project
    aid#7 = a.id#7
    bid#14 = b.id#14
    NestedLoopJoin inner on eq(a.id#7, b.id#14)
      Ref top
      Ref top
`)
}

// Strategy selection: one occurrence streams inline (replay — the single
// evaluation is the commitment); two or more materialise once, so nested
// multi-reference bindings execute linearly.
func TestPlanBindingStrategy(t *testing.T) {
	q := bind(t, lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"once":  bfilter(bscan("tasks", "t"), beq(bcol("t", "status"), blit("open"))),
			"twice": bfilter(bscan("tasks", "u"), beq(bcol("u", "status"), blit("done"))),
		},
		Root: lir.Project{
			Input: lir.Join{
				Left: lir.Ref{Binding: "once", Scope: "o"},
				Right: lir.Join{
					Left:  lir.Ref{Binding: "twice", Scope: "a"},
					Right: lir.Ref{Binding: "twice", Scope: "b"},
					Kind:  lir.InnerJoin,
					On:    beq(bcol("a", "id"), bcol("b", "id")),
				},
				Kind: lir.InnerJoin,
				On:   beq(bcol("o", "id"), bcol("a", "id")),
			},
			Scope:  "j",
			Fields: []lir.ProjField{{As: "oid", Expr: bcol("o", "id")}, {As: "aid", Expr: bcol("a", "id")}, {As: "bid", Expr: bcol("b", "id")}},
		},
	})
	pp := planner.PlanQuery(q)
	want := map[string]planner.BindingStrategy{
		"once":  planner.BindingReplay,
		"twice": planner.BindingMaterialise,
	}
	for _, bp := range pp.Bindings {
		if bp.Strategy != want[bp.Name] {
			t.Fatalf("binding %q strategy = %s, want %s", bp.Name, bp.Strategy, want[bp.Name])
		}
	}
}

// Sensitivity classification: a slice is conservatively sensitive; a plain
// filter is not; a reference to a sensitive binding is NOT itself sensitive
// (it observes an already-committed value).
func TestPlanBindingSensitivity(t *testing.T) {
	q := bind(t, lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"arb":  lir.Slice{Input: bscan("tasks", "t"), Limit: new(int(2))},
			"det":  bfilter(bscan("tasks", "u"), beq(bcol("u", "status"), blit("open"))),
			"over": bfilter(lir.Ref{Binding: "arb", Scope: "r"}, beq(bcol("r", "status"), blit("open"))),
		},
		Root: lir.Project{
			Input: lir.Join{
				Left: lir.Ref{Binding: "det", Scope: "d"},
				Right: lir.Join{
					Left:  lir.Ref{Binding: "over", Scope: "o"},
					Right: lir.Ref{Binding: "arb", Scope: "a"},
					Kind:  lir.InnerJoin,
					On:    beq(bcol("o", "id"), bcol("a", "id")),
				},
				Kind: lir.InnerJoin,
				On:   beq(bcol("d", "id"), bcol("o", "id")),
			},
			Scope:  "j",
			Fields: []lir.ProjField{{As: "did", Expr: bcol("d", "id")}, {As: "oid", Expr: bcol("o", "id")}, {As: "aid", Expr: bcol("a", "id")}},
		},
	})
	want := map[string]bool{"arb": true, "det": false, "over": false}
	for _, b := range q.Bindings {
		if b.PlanSensitive != want[b.Name] {
			t.Fatalf("binding %q sensitivity = %v, want %v", b.Name, b.PlanSensitive, want[b.Name])
		}
	}
}
