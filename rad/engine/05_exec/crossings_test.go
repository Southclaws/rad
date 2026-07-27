package exec

// The composability gates: a crossing is a value wherever an expression can
// stand — filter predicates, arithmetic, order terms — and wrapping it in a
// wider expression cannot change how it executes. These are the acceptance
// tests for crossing extraction and the unified runtime datum.

import (
	"reflect"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// countTasks is scalar(count tasks of this board) — the classic fold.
func countTasks(boardScope string) lir.Expr {
	return lir.Scalar{Rel: lir.Aggregate{
		Input: qfilter(qscan("tasks", "ct"),
			qeq(qcol("ct", "board_id"), qcol(boardScope, "id"))),
		Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	}}
}

// A crossing in a filter predicate — and negated, which once meant a
// different execution path. Both directions partition the same set.
func TestCrossingInFilterPredicate(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	hasComments := lir.Exists{Rel: qfilter(qscan("comments", "c"),
		qeq(qcol("c", "task_id"), qcol("t", "id")))}

	with, err := eng.Execute(ctx, many(lir.Order{
		Input: qfilter(qscan("tasks", "t"), hasComments),
		Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
	}))
	if err != nil {
		t.Fatal(err)
	}
	without, err := eng.Execute(ctx, many(lir.Order{
		Input: qfilter(qscan("tasks", "t"), lir.Unary{Op: lir.OpNot, X: hasComments}),
		Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
	}))
	if err != nil {
		t.Fatal(err)
	}

	ids := func(d lir.Datum) []any {
		var out []any
		for _, e := range d.Elems {
			out = append(out, jsonish(e).(map[string]any)["id"])
		}
		return out
	}
	if got := ids(with); !reflect.DeepEqual(got, []any{"t1", "t4"}) {
		t.Fatalf("tasks with comments = %v", got)
	}
	if got := ids(without); !reflect.DeepEqual(got, []any{"t2", "t3"}) {
		t.Fatalf("tasks without comments = %v", got)
	}
}

// Wrapping a crossing in a wider expression changes the value, not the
// execution: the wrapped query issues exactly the same storage operations.
func TestWrappingKeepsStorageWork(t *testing.T) {
	eng, ctx, gets, scans := lirSetup(t)

	project := func(countExpr lir.Expr) lir.Query {
		return many(lir.Project{
			Input:  qscan("boards", "b"),
			Fields: []lir.ProjField{{As: "id", Expr: qcol("b", "id")}, {As: "n", Expr: countExpr}},
		})
	}

	*gets, *scans = 0, 0
	plain, err := eng.Execute(ctx, project(countTasks("b")))
	if err != nil {
		t.Fatal(err)
	}
	plainGets, plainScans := *gets, *scans

	*gets, *scans = 0, 0
	wrapped, err := eng.Execute(ctx, project(
		lir.Binary{Op: lir.OpAdd, L: countTasks("b"), R: qlit(1)}))
	if err != nil {
		t.Fatal(err)
	}
	if *gets != plainGets || *scans != plainScans {
		t.Fatalf("wrapping changed storage work: plain %d gets/%d scans, wrapped %d/%d",
			plainGets, plainScans, *gets, *scans)
	}

	for i, e := range plain.Elems {
		n := jsonish(e).(map[string]any)["n"].(int64)
		w := jsonish(wrapped.Elems[i]).(map[string]any)["n"].(int64)
		if w != n+1 {
			t.Fatalf("board %d: wrapped n = %d, plain n = %d", i, w, n)
		}
	}
}

// A first output is a value: reference it through the projection's scope,
// test it for NULL in a filter, and reproject it above — the row shape
// survives untouched.
func TestNestedOutputIsReferable(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	withAssignee := lir.Project{
		Input: qscan("tasks", "t"),
		Scope: "p",
		Fields: []lir.ProjField{
			{As: "id", Expr: qcol("t", "id")},
			{As: "assignee", Expr: lir.First{Rel: qfilter(qscan("users", "u"),
				qeq(qcol("u", "id"), qcol("t", "assignee_id")))}},
		},
	}

	unassigned, err := eng.Execute(ctx, many(lir.Filter{
		Input: withAssignee,
		Pred:  lir.Unary{Op: lir.OpIsNull, X: qcol("p", "assignee")},
	}))
	if err != nil {
		t.Fatal(err)
	}
	want := []any{map[string]any{"id": "t2", "assignee": nil}}
	if got := jsonish(unassigned); !reflect.DeepEqual(got, want) {
		t.Fatalf("unassigned = %#v, want %#v", got, want)
	}

	// Reprojection above the filter: the nested object flows through slots
	// like any scalar, renamed and intact.
	reprojected, err := eng.Execute(ctx, many(lir.Project{
		Input: lir.Filter{
			Input: withAssignee,
			Pred:  lir.Unary{Op: lir.OpIsNotNull, X: qcol("p", "assignee")},
		},
		Fields: []lir.ProjField{
			{As: "task", Expr: qcol("p", "id")},
			{As: "who", Expr: qcol("p", "assignee")},
		},
	}))
	if err != nil {
		t.Fatal(err)
	}
	if len(reprojected.Elems) != 3 {
		t.Fatalf("assigned tasks = %d, want 3", len(reprojected.Elems))
	}
	first := jsonish(reprojected.Elems[0]).(map[string]any)
	who, ok := first["who"].(map[string]any)
	if !ok || who["name"] == nil {
		t.Fatalf("reprojected nested object lost its shape: %#v", first)
	}
}

// A crossing as an order term: boards by task count, descending, with the
// binder's unique-key tie-breaker keeping ties deterministic.
func TestCrossingAsOrderTerm(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	d, err := eng.Execute(ctx, many(lir.Project{
		Input: lir.Order{
			Input: qscan("boards", "b"),
			Terms: []lir.OrderTerm{{Expr: countTasks("b"), Desc: true}},
		},
		Spread: []string{"b"},
	}))
	if err != nil {
		t.Fatal(err)
	}
	var ids []any
	for _, e := range d.Elems {
		ids = append(ids, jsonish(e).(map[string]any)["id"])
	}
	if !reflect.DeepEqual(ids, []any{"b1", "b2", "b3"}) {
		t.Fatalf("boards by task count = %v", ids)
	}
}
