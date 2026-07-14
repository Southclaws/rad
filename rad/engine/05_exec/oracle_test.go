package exec

// The reference-interpreter differential: every real execution must agree with
// the naive interpreter in rad/engine/05_exec/refexec, which pins what the
// answer actually IS (where path-independence only proves the engine equivalent
// to itself under different physical choices). This file binds each query once
// and runs the same bound query through both the engine and the interpreter —
// the split is after binding, so a binder difference can't masquerade as an
// executor difference.

import (
	"context"
	"reflect"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	"github.com/Southclaws/rad/rad/engine/05_exec/refexec"
)

func TestReferenceInterpreter(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	queries := conformanceQueries()
	queries["wrapped crossing arithmetic"] = many(lir.Project{
		Input: qscan("boards", "b"),
		Fields: []lir.ProjField{
			{As: "id", Expr: qcol("b", "id")},
			{As: "n", Expr: lir.Binary{Op: lir.OpAdd, L: countTasks("b"), R: qlit(1)}},
		},
	})
	queries["left join"] = many(lir.Order{
		// tasks and users both expose `id`; project to a unique output. An
		// unmatched left row still leaves `who` NULL, exercising the left pad.
		Input: lir.Project{
			Input: lir.Join{
				Left:  qscan("tasks", "t"),
				Right: qscan("users", "u"),
				Kind:  lir.LeftJoin,
				On:    qeq(qcol("t", "assignee_id"), qcol("u", "id")),
			},
			Scope: "j",
			Fields: []lir.ProjField{
				{As: "id", Expr: qcol("t", "id")},
				{As: "who", Expr: qcol("u", "name")},
			},
		},
		Terms: []lir.OrderTerm{{Expr: qcol("j", "id")}},
	})

	for name, q := range queries {
		t.Run(name, func(t *testing.T) {
			engine, err := eng.Execute(ctx, q)
			if err != nil {
				t.Fatal(err)
			}
			oracle, err := interpQuery(ctx, eng, q)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(jsonish(engine), jsonish(oracle)) {
				t.Fatalf("engine disagrees with the oracle.\n engine: %#v\n oracle: %#v",
					jsonish(engine), jsonish(oracle))
			}
		})
	}
}

// interpQuery binds q with the shared production binder, then evaluates the
// bound query with the reference interpreter, feeding it stored rows through a
// scan closure so the interpreter itself touches no storage.
func interpQuery(ctx context.Context, e *Engine, q lir.Query) (lir.Datum, error) {
	bq, err := planner.Bind(ctx, e.cat, q)
	if err != nil {
		return lir.Datum{}, err
	}
	scan := func(ctx context.Context, tbl catalog.Table) ([]lir.Row, error) {
		it, err := scanTable(ctx, e.store, tbl)
		if err != nil {
			return nil, err
		}
		defer it.Close()
		var rows []lir.Row
		for {
			row, ok, err := it.Next()
			if err != nil {
				return nil, err
			}
			if !ok {
				return rows, nil
			}
			rows = append(rows, row)
		}
	}
	return refexec.Interpret(ctx, scan, bq)
}
