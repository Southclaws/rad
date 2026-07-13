package exec

// Path independence: the load-bearing invariant of the whole planner. For
// any query, the chosen access paths and the forced-full-scan plan must
// produce identical results — the residual filter is the source of truth,
// and physical choices can never change what a query means. Together with
// batched ≡ nested (execute_test.go), this is the conformance core.

import (
	"context"
	"reflect"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

// conformanceQueries is the shared corpus: every query shape the invariants
// are held over, by path independence here and by the reference interpreter
// in oracle_test.go.
func conformanceQueries() map[string]lir.Query {
	return map[string]lir.Query{
		"forcing query": forcingQ(),
		"pk lookup": many(qfilter(qscan("tasks", "t"),
			qeq(qcol("t", "id"), qlit("t2")))),
		"index prefix": many(lir.Order{
			Input: qfilter(qscan("tasks", "t"),
				qand(qeq(qcol("t", "board_id"), qlit("b1")),
					qeq(qcol("t", "status"), qlit("open")))),
			Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
		}),
		"range": many(lir.Order{
			Input: qfilter(qscan("tasks", "t"),
				qand(qeq(qcol("t", "board_id"), qlit("b1")),
					lir.Binary{Op: lir.OpGte, L: qcol("t", "status"), R: qlit("m")})),
			Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
		}),
		"ordered pushdown": many(lir.Slice{
			Input: lir.Order{
				Input: qscan("users", "u"),
				Terms: []lir.OrderTerm{{Expr: qcol("u", "name")}},
			},
			Limit: new(int(1)),
		}),
		"grouped fold": many(lir.Order{
			Input: lir.Aggregate{
				Input:  qscan("tasks", "t"),
				Scope:  "stats",
				Groups: []lir.GroupTerm{{Expr: qcol("t", "status")}},
				Terms:  []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
			},
			Terms: []lir.OrderTerm{{Expr: qcol("stats", "status")}},
		}),
		"crossing in a filter": many(lir.Order{
			Input: qfilter(qscan("tasks", "t"),
				lir.Unary{Op: lir.OpNot, X: lir.Exists{Rel: qfilter(qscan("comments", "c"),
					qeq(qcol("c", "task_id"), qcol("t", "id")))}}),
			Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
		}),
		"nested output referenced above": many(lir.Filter{
			Input: lir.Project{
				Input: qscan("tasks", "t"),
				Scope: "p",
				Fields: []lir.ProjField{
					{As: "id", Expr: qcol("t", "id")},
					{As: "assignee", Expr: lir.First{Rel: qfilter(qscan("users", "u"),
						qeq(qcol("u", "id"), qcol("t", "assignee_id")))}},
				},
			},
			Pred: lir.Unary{Op: lir.OpIsNotNull, X: qcol("p", "assignee")},
		}),
		"binding self-join": {
			Card: lir.CardMany,
			Bindings: map[string]lir.Relation{
				"b1tasks": qfilter(qscan("tasks", "t"),
					qeq(qcol("t", "board_id"), qlit("b1"))),
			},
			Root: lir.Order{
				Input: lir.Join{
					Left:  lir.Ref{Binding: "b1tasks", Scope: "a"},
					Right: lir.Ref{Binding: "b1tasks", Scope: "b"},
					Kind:  lir.InnerJoin,
					On:    qeq(qcol("a", "id"), qcol("b", "id")),
				},
				Terms: []lir.OrderTerm{{Expr: qcol("a", "id")}},
			},
		},
	}
}

func TestPathIndependence(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	for name, q := range conformanceQueries() {
		t.Run(name, func(t *testing.T) {
			chosen, err := eng.Execute(ctx, q)
			if err != nil {
				t.Fatal(err)
			}
			forced, err := executeFullScan(ctx, eng, q)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(jsonish(chosen), jsonish(forced)) {
				t.Fatalf("access path changed results.\n chosen: %#v\n forced: %#v",
					jsonish(chosen), jsonish(forced))
			}
		})
	}
}

// executeFullScan runs a query with every access forced to a filtered table
// scan.
func executeFullScan(ctx context.Context, e *Engine, q lir.Query) (lir.Datum, error) {
	bq, err := planner.Bind(ctx, e.cat, q)
	if err != nil {
		return lir.Datum{}, err
	}
	pp := planner.PlanQuery(bq, planner.FullScanOnly())

	ex := newExecutor(e.store)
	if err := ex.commit(ctx, pp.Bindings); err != nil {
		return lir.Datum{}, err
	}
	op, err := ex.build(ctx, pp.Root, bound.Env{})
	if err != nil {
		return lir.Datum{}, err
	}
	defer op.Close()

	frames, err := drainOp(ctx, op)
	if err != nil {
		return lir.Datum{}, err
	}
	elems := make([]lir.Datum, len(frames))
	for i, f := range frames {
		elems[i] = frameToObject(pp.Out, f)
	}
	return lir.ArrayDatum(elems), nil
}
