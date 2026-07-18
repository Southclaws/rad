package gen

// The generative differential as a first-class test tool, composing two
// reusable frameworks: the generator (rad/engine/05_exec/generative) invents a
// catalog, data, and a correct-by-construction query, and the differential
// (rad/engine/05_exec/differential) runs that query the engine's chosen way,
// forced to full scans, and through the reference interpreter, requiring all
// three to agree. Generation draws from a rapid.T, so a failing case minimises
// automatically. This file is only the glue: build a database, load the data,
// and hand each query to the differential.
//
// The interpreter is fed the exact rows loaded here, so the comparison also
// pins the insert -> encode -> decode round-trip. Two sources feed it: a
// synthetic random catalog, and a directory of rad.schema.yaml scenarios the
// generator runs against once it introspects one into a shape it drives (single
// text "id" key, nullable text FKs, non-unique indexes); a schema outside that
// shape is skipped with the reason. New scenarios are new directories.

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"

	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	differential "github.com/Southclaws/rad/rad/engine/05_exec/differential"
	generative "github.com/Southclaws/rad/rad/engine/05_exec/generative"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	emit "github.com/Southclaws/rad/tests/gen/emit"
	"pgregory.net/rapid"
)

func TestGenerativeSynthetic(t *testing.T) {
	ctx := context.Background()
	capt := newCapture(t, "synthetic", nil)
	before := casesChecked.Load()
	rapid.Check(t, func(rt *rapid.T) {
		spec := generative.SynthCatalog(rt)
		db, done := freshDB(t)
		defer done()
		for _, def := range generative.TableDefs(spec) {
			if _, err := db.CreateTable(ctx, def); err != nil {
				rt.Fatalf("create table %q: %v", def.Name, err)
			}
		}
		runOne(rt, ctx, db, spec, capt)
	})
	t.Logf("[generative] synthetic: %d cases checked", casesChecked.Load()-before)
}

func TestGenerativeSchemas(t *testing.T) {
	ctx := context.Background()
	entries, err := os.ReadDir(".")
	if err != nil {
		t.Fatalf("read tests/gen: %v", err)
	}
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		path := filepath.Join(e.Name(), "rad.schema.yaml")
		src, err := os.ReadFile(path)
		if os.IsNotExist(err) {
			continue // not a scenario directory
		}
		if err != nil {
			t.Fatalf("read %s: %v", path, err)
		}
		t.Run(e.Name(), func(t *testing.T) {
			// Introspect once into a generator spec; it then serves every case.
			spec := introspectSchema(t, ctx, path, src)
			capt := newCapture(t, e.Name(), src)
			before := casesChecked.Load()
			rapid.Check(t, func(rt *rapid.T) {
				db, done := freshDB(t)
				defer done()
				if _, err := db.MigrateFile(ctx, path, src); err != nil {
					rt.Fatalf("migrate: %v", err)
				}
				runOne(rt, ctx, db, spec, capt)
			})
			t.Logf("[generative] %s: %d cases checked", e.Name(), casesChecked.Load()-before)
		})
	}
}

// runOne loads a generated dataset, generates one query (bag or ordered), and
// checks the engine against the reference interpreter fed that dataset. On a
// divergence it records the case so a fixture can be emitted, then fails —
// letting rapid shrink and re-run, so the last recorded case is the minimal one.
func runOne(rt *rapid.T, ctx context.Context, db *frontend.DB, spec *generative.Catalog, capt *capture) {
	data := generative.GenerateData(rt, spec)
	for _, tbl := range spec.Tables {
		for _, row := range data[tbl.Name] {
			if err := db.Insert(ctx, tbl.Name, row); err != nil {
				rt.Fatalf("insert into %q: %v", tbl.Name, err)
			}
		}
	}
	g := generative.NewGenerator(rt, spec)
	ordered := rapid.Bool().Draw(rt, "ordered")
	q := g.Query()
	if ordered {
		q = g.OrderedQuery()
	}
	casesChecked.Add(1)
	if dumpEnabled() {
		dumpCase(capt.mode, dumpSchema(capt, spec), q, dumpResult(ctx, db, q))
	}
	if err := differential.ThreeWay(ctx, db, scanOf(data), q, ordered); err != nil {
		capt.record(emit.Case{Spec: spec, Data: data, Query: q, Ordered: ordered, Detail: err.Error()})
		rt.Fatal(err)
	}
}

// dumpSchema renders the case's schema for the run dump — the original source
// for a schema-directed case, or the serialised synthetic spec.
func dumpSchema(capt *capture, spec *generative.Catalog) string {
	if len(capt.schemaSrc) > 0 {
		return string(capt.schemaSrc)
	}
	return emit.SchemaYAML(spec)
}

// dumpResult executes q for the run dump, recording the error instead when the
// query legitimately fails at runtime (e.g. checked overflow).
func dumpResult(ctx context.Context, db *frontend.DB, q lir.Query) any {
	res, err := db.Execute(ctx, q)
	if err != nil {
		return map[string]string{"error": err.Error()}
	}
	return frontend.DatumJSON(res)
}

// capture holds the latest failing case and, when RAD_GEN_EMIT is set, writes
// it to the e2e suite as a permanent regression fixture once the test finishes
// red. Because rapid re-runs the property while shrinking, the last recorded
// case is the minimal one. Without the env var the differential just fails
// (fast) — capture is opt-in so a failing run doesn't silently litter fixtures.
type capture struct {
	mode      string
	schemaSrc []byte
	last      *emit.Case
}

// record keeps the case, stamping it with this capture's mode and (for a
// schema-directed run) the original schema source to copy verbatim.
func (c *capture) record(cs emit.Case) {
	cs.Mode = c.mode
	cs.SchemaSrc = c.schemaSrc
	c.last = &cs
}

func newCapture(t *testing.T, mode string, schemaSrc []byte) *capture {
	c := &capture{mode: mode, schemaSrc: schemaSrc}
	t.Cleanup(func() {
		out := os.Getenv("RAD_GEN_EMIT")
		if out == "" || !t.Failed() || c.last == nil {
			return
		}
		if out == "1" { // the default target: the e2e suite next door
			out = filepath.Join("..", "e2e")
		}
		dir, err := emit.Fixture(context.Background(), out, *c.last)
		if err != nil {
			t.Logf("emit fixture: %v", err)
			return
		}
		t.Logf("emitted regression fixture: %s", dir)
	})
	return c
}

// introspectSchema migrates a schema into a throwaway database and introspects
// it into a generator spec.
func introspectSchema(t *testing.T, ctx context.Context, path string, src []byte) *generative.Catalog {
	t.Helper()
	db, done := freshDB(t)
	defer done()
	if _, err := db.MigrateFile(ctx, path, src); err != nil {
		t.Fatalf("migrate: %v", err)
	}
	tables, err := db.Tables(ctx)
	if err != nil {
		t.Fatalf("introspect: %v", err)
	}
	return generative.Introspect(tables)
}

// scanOf feeds the reference interpreter the rows the runner inserted, keyed by
// table.
func scanOf(data map[string][]lir.Row) differential.ScanFunc {
	return func(_ context.Context, tbl catalog.Table) ([]lir.Row, error) {
		return data[tbl.Name], nil
	}
}

var dbSeq atomic.Int64

// freshDB opens a fresh in-memory database. Each call gets a distinct name so
// repeated opens under one test (rapid runs many iterations) never share state.
func freshDB(t *testing.T) (*frontend.DB, func()) {
	t.Helper()
	store, err := kvslate.Open(fmt.Sprintf("gen-%d", dbSeq.Add(1)), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	return frontend.Open(store), func() { _ = store.Close() }
}
