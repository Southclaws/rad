package exec

// Replay determinism: executing the same query against the same statement
// snapshot yields the exact same result — not just the same bag, the same
// Datum, field order and all. This is stronger than path independence
// (which compares different plans) and is its own load-bearing invariant:
// deterministic replay, cache keys, explain verification, and the future
// binding construct's identical-plan discharge all consume it. Go
// randomises map iteration per range loop, so in-process repetition
// genuinely probes for map-order leaks anywhere in the pipeline.

import (
	"reflect"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// replayOnlyQueries are plan-choice-sensitive shapes: two valid physical
// plans may legitimately select different legal bags (arbitrary membership,
// ties above a keyless projection, join encounter order), so they can never
// join the path-independence corpus — replay determinism is exactly the
// property that still holds for them, and the one the binding construct's
// identical-plan discharge depends on.
func replayOnlyQueries() map[string]lir.Query {
	return map[string]lir.Query{
		"arbitrary slice": many(lir.Slice{
			Input: qscan("tasks", "t"),
			Limit: new(int(2)),
		}),
		// The projection drops the primary key, so the binder has no unique
		// tie-breaker to append: the order is genuinely tied.
		"tied order over keyless projection": many(lir.Slice{
			Input: lir.Order{
				Input: lir.Project{
					Input:  qscan("tasks", "t"),
					Scope:  "p",
					Fields: []lir.ProjField{{As: "s", Expr: qcol("t", "status")}},
				},
				Terms: []lir.OrderTerm{{Expr: qcol("p", "s")}},
			},
			Limit: new(int(2)),
		}),
		"join encounter order": many(lir.Project{
			// Unordered on purpose (encounter-order sensitive); project past
			// the tasks/users `id` collision without imposing an order.
			Input: lir.Join{
				Left:  qscan("tasks", "t"),
				Right: qscan("users", "u"),
				Kind:  lir.InnerJoin,
				On:    qeq(qcol("t", "assignee_id"), qcol("u", "id")),
			},
			Scope:  "j",
			Fields: []lir.ProjField{{As: "tid", Expr: qcol("t", "id")}, {As: "uid", Expr: qcol("u", "id")}},
		}),
	}
}

func TestReplayDeterminism(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	queries := conformanceQueries()
	for name, q := range replayOnlyQueries() {
		queries["sensitive: "+name] = q
	}
	for name, q := range queries {
		t.Run(name, func(t *testing.T) {
			first, err := eng.Execute(ctx, q)
			if err != nil {
				t.Fatal(err)
			}
			for i := 1; i < 10; i++ {
				again, err := eng.Execute(ctx, q)
				if err != nil {
					t.Fatal(err)
				}
				if !reflect.DeepEqual(first, again) {
					t.Fatalf("replay %d diverged.\nfirst: %#v\nagain: %#v",
						i, jsonish(first), jsonish(again))
				}
			}
		})
	}
}
