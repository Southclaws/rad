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

	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
	planner "rad/rad/04_planner"
)

func TestPathIndependence(t *testing.T) {
	eng, ctx, _, _ := qirSetup(t)

	queries := map[string]qir.Query{
		"forcing query": forcingQ(),
		"pk lookup": many(qfilter(qscan("tasks", "t"),
			qeq(qcol("t", "id"), qlit("t2")))),
		"index prefix": many(qir.Order{
			Input: qfilter(qscan("tasks", "t"),
				qand(qeq(qcol("t", "board_id"), qlit("b1")),
					qeq(qcol("t", "status"), qlit("open")))),
			Terms: []qir.OrderTerm{{Expr: qcol("t", "id")}},
		}),
		"range": many(qir.Order{
			Input: qfilter(qscan("tasks", "t"),
				qand(qeq(qcol("t", "board_id"), qlit("b1")),
					qir.Binary{Op: qir.OpGte, L: qcol("t", "status"), R: qlit("m")})),
			Terms: []qir.OrderTerm{{Expr: qcol("t", "id")}},
		}),
		"ordered pushdown": many(qir.Slice{
			Input: qir.Order{
				Input: qscan("users", "u"),
				Terms: []qir.OrderTerm{{Expr: qcol("u", "name")}},
			},
			Limit: new(int(1)),
		}),
		"grouped fold": many(qir.Order{
			Input: qir.Aggregate{
				Input:  qscan("tasks", "t"),
				Scope:  "stats",
				Groups: []qir.GroupTerm{{Expr: qcol("t", "status")}},
				Terms:  []qir.AggTerm{{Fn: qir.AggCount, As: "n"}},
			},
			Terms: []qir.OrderTerm{{Expr: qcol("stats", "status")}},
		}),
	}

	for name, q := range queries {
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
func executeFullScan(ctx context.Context, e *Engine, q qir.Query) (qir.Datum, error) {
	bq, err := planner.Bind(ctx, e.cat, q)
	if err != nil {
		return qir.Datum{}, err
	}
	pp := planner.PlanQuery(bq, planner.FullScanOnly())

	ex := newExecutor(e.store)
	op, err := ex.build(ctx, pp.Root, bound.Env{})
	if err != nil {
		return qir.Datum{}, err
	}
	defer op.Close()

	frames, err := drainOp(ctx, op)
	if err != nil {
		return qir.Datum{}, err
	}
	elems := make([]qir.Datum, len(frames))
	for i, f := range frames {
		elems[i] = frameToObject(pp.Out, f)
	}
	return qir.ArrayDatum(elems), nil
}
