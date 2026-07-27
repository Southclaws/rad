package gen

// The generative differential for recursive queries: the generator invents a
// random directed graph and a correct-by-construction recursive query over it
// (random anchor set, union mode, and recursive-state signature), and the
// differential holds the engine — chosen plan and forced full scan — to the
// reference interpreter fed the same edges. Generation draws from a rapid.T, so
// a divergence shrinks to a minimal graph and query automatically. Comparison
// is a multiset: a recursive relation has no inherent order, and `union all`
// duplicates would make a sequence comparison spuriously fail.

import (
	"context"
	"testing"

	differential "github.com/Southclaws/rad/rad/engine/05_exec/differential"
	generative "github.com/Southclaws/rad/rad/engine/05_exec/generative"
	emit "github.com/Southclaws/rad/tests/gen/emit"
	"pgregory.net/rapid"
)

func TestGenerativeRecursive(t *testing.T) {
	ctx := context.Background()
	capt := newCapture(t, "recursive", nil)
	before := casesChecked.Load()
	spec := generative.GraphCatalog()
	rapid.Check(t, func(rt *rapid.T) {
		db, done := freshDB(t)
		defer done()
		for _, def := range generative.TableDefs(spec) {
			if _, err := db.CreateTable(ctx, def); err != nil {
				rt.Fatalf("create table %q: %v", def.Name, err)
			}
		}

		g := generative.GenGraph(rt)
		data := generative.GraphData(g)
		for _, row := range data["edges"] {
			if err := db.Insert(ctx, "edges", row); err != nil {
				rt.Fatalf("insert edge: %v", err)
			}
		}

		q := generative.GenRecursiveQuery(rt, g)
		casesChecked.Add(1)
		if dumpEnabled() {
			dumpCase(capt.mode, emit.SchemaYAML(spec), q, dumpResult(ctx, db, q))
		}
		if err := differential.ThreeWay(ctx, db, scanOf(data), q, false); err != nil {
			capt.record(emit.Case{Spec: spec, Data: data, Query: q, Ordered: false, Detail: err.Error()})
			rt.Fatal(err)
		}
	})
	t.Logf("[generative] recursive: %d cases checked", casesChecked.Load()-before)
}
