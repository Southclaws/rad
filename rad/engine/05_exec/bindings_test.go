package exec

// Relation bindings: the acceptance semantics. A binding denotes one
// statement-local committed value; every reference observes the same bag
// under fresh occurrence slots. The diagonal test is the load-bearing one:
// it only passes if a nondeterministic body is committed once — under
// macro (re-evaluation) semantics the two sides could be different bags.

import (
	"strings"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// TestBindingCommitDiagonal: self-join an arbitrary two-row slice on the
// full primary key. Both occurrences must observe the same committed bag,
// so the join is exactly the diagonal: two rows, each equal on both sides.
func TestBindingCommitDiagonal(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	q := lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"top": lir.Slice{Input: qscan("tasks", "t"), Limit: new(int(2))},
		},
		Root: lir.Project{
			Input: lir.Order{
				Input: lir.Join{
					Left:  lir.Ref{Binding: "top", Scope: "a"},
					Right: lir.Ref{Binding: "top", Scope: "b"},
					Kind:  lir.InnerJoin,
					On:    qeq(qcol("a", "id"), qcol("b", "id")),
				},
				Terms: []lir.OrderTerm{{Expr: qcol("a", "id")}},
			},
			Fields: []lir.ProjField{
				{As: "left", Expr: qcol("a", "id")},
				{As: "right", Expr: qcol("b", "id")},
			},
		},
	}

	d, err := eng.Execute(ctx, q)
	if err != nil {
		t.Fatal(err)
	}
	if len(d.Elems) != 2 {
		t.Fatalf("diagonal self-join of an any-2 binding = %d rows, want exactly 2\n%v",
			len(d.Elems), jsonish(d))
	}
	seen := map[any]bool{}
	for _, el := range d.Elems {
		row := jsonish(el).(map[string]any)
		if row["left"] != row["right"] {
			t.Fatalf("off-diagonal row %v: the occurrences observed different bags", row)
		}
		seen[row["left"]] = true
	}
	if len(seen) != 2 {
		t.Fatalf("diagonal ids not distinct: %v", seen)
	}
}

// TestBindingAliasIsolation: occurrences of one deterministic binding keep
// independent slots — identically named columns on both sides of a
// self-join do not bleed into each other.
func TestBindingAliasIsolation(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	q := lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"b1tasks": lir.Filter{Input: qscan("tasks", "t"),
				Pred: qeq(qcol("t", "board_id"), qlit("b1"))},
		},
		Root: lir.Project{
			Input: lir.Order{
				Input: lir.Join{
					Left:  lir.Ref{Binding: "b1tasks", Scope: "a"},
					Right: lir.Ref{Binding: "b1tasks", Scope: "b"},
					Kind:  lir.InnerJoin,
					On:    qeq(qcol("a", "id"), qcol("b", "id")),
				},
				Terms: []lir.OrderTerm{{Expr: qcol("a", "id")}},
			},
			Fields: []lir.ProjField{
				{As: "a_id", Expr: qcol("a", "id")},
				{As: "b_id", Expr: qcol("b", "id")},
				{As: "a_title", Expr: qcol("a", "title")},
				{As: "b_title", Expr: qcol("b", "title")},
			},
		},
	}

	d, err := eng.Execute(ctx, q)
	if err != nil {
		t.Fatal(err)
	}
	// Board b1 has t1, t2, t3: the diagonal has three rows, sides equal.
	if len(d.Elems) != 3 {
		t.Fatalf("rows = %d, want 3\n%v", len(d.Elems), jsonish(d))
	}
	for _, el := range d.Elems {
		row := jsonish(el).(map[string]any)
		if row["a_id"] != row["b_id"] || row["a_title"] != row["b_title"] {
			t.Fatalf("occurrence slots bled: %v", row)
		}
	}
}

// TestBindingHiddenScope: the binding's interior scopes are not visible
// through a reference — only the occurrence's own alias is.
func TestBindingHiddenScope(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	q := lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"all": lir.Filter{Input: qscan("tasks", "t"),
				Pred: qeq(qcol("t", "status"), qlit("open"))},
		},
		Root: lir.Filter{
			Input: lir.Ref{Binding: "all", Scope: "r"},
			// "t" is interior to the binding: invisible out here.
			Pred: qeq(qcol("t", "title"), qlit("ship")),
		},
	}
	_, err := eng.Execute(ctx, q)
	if err == nil || !strings.Contains(err.Error(), `scope "t" exists but is not visible`) {
		t.Fatalf("interior scope leaked through the reference: err = %v", err)
	}

	// The occurrence alias works where the interior scope does not.
	q.Root = lir.Filter{
		Input: lir.Ref{Binding: "all", Scope: "r"},
		Pred:  qeq(qcol("r", "title"), qlit("ship")),
	}
	d, err := eng.Execute(ctx, q)
	if err != nil {
		t.Fatal(err)
	}
	if len(d.Elems) != 1 {
		t.Fatalf("alias filter rows = %d, want 1", len(d.Elems))
	}
}

// TestBindingChain: a binding may reference an earlier binding; commitment
// runs in dependency order.
func TestBindingChain(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	q := lir.Query{
		Card: lir.CardExactlyOne,
		Bindings: map[string]lir.Relation{
			"open": lir.Filter{Input: qscan("tasks", "t"),
				Pred: qeq(qcol("t", "status"), qlit("open"))},
			"stats": lir.Aggregate{
				Input: lir.Ref{Binding: "open", Scope: "o"},
				Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
			},
		},
		Root: lir.Ref{Binding: "stats", Scope: "s"},
	}
	d, err := eng.Execute(ctx, q)
	if err != nil {
		t.Fatal(err)
	}
	got := jsonish(d).(map[string]any)
	if got["n"] != int64(3) {
		t.Fatalf("chained binding count = %v, want 3", got["n"])
	}
}

// TestBindingRefIsManyRows: references are uniformly 0..many — a scalar
// root over a reference is rejected even when the body is a global fold.
func TestBindingRefIsManyRows(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	q := lir.Query{
		Card: lir.CardScalar,
		Bindings: map[string]lir.Relation{
			"n": lir.Aggregate{Input: qscan("tasks", "t"),
				Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}}},
		},
		Root: lir.Ref{Binding: "n", Scope: "s"},
	}
	_, err := eng.Execute(ctx, q)
	if err == nil || !strings.Contains(err.Error(), "at most one row") {
		t.Fatalf("scalar over a 0..many reference accepted: err = %v", err)
	}
}
