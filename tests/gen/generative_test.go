package gen

// The generative differential as a first-class test tool, composing two
// reusable frameworks: the generator (rad/engine/05_exec/generative) invents a
// catalog, data, and a correct-by-construction query, and the differential
// (rad/engine/05_exec/differential) runs that query the engine's chosen way,
// forced to full scans, and through the reference interpreter, requiring all
// three to agree. This file is only the glue: build a database, load the data,
// and hand each query to the differential.
//
// The interpreter is fed the exact rows loaded here, so the comparison also
// pins the insert -> encode -> decode round-trip. Two sources feed it: a
// synthetic random catalog, and a directory of schema.rad scenarios the
// generator runs against once it introspects one into a shape it drives (single
// text "id" key, nullable text FKs, non-unique indexes); a schema outside that
// shape is skipped with the reason. New scenarios are new directories.

import (
	"context"
	"math/rand"
	"os"
	"path/filepath"
	"strconv"
	"testing"

	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	differential "github.com/Southclaws/rad/rad/engine/05_exec/differential"
	generative "github.com/Southclaws/rad/rad/engine/05_exec/generative"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

// seeds is how many cases each source explores per mode. Modest by default so
// `go test ./...` stays quick; env-tunable for a deeper soak (RAD_GEN_SEEDS).
func seeds() int {
	if s := os.Getenv("RAD_GEN_SEEDS"); s != "" {
		if n, err := strconv.Atoi(s); err == nil && n > 0 {
			return n
		}
	}
	return 50
}

// mode is a query flavour: a bag compared as a multiset, or a totally ordered
// sequence compared element by element (catching ordering bugs a bag misses).
type mode struct {
	name    string
	ordered bool
	gen     func(*generative.Generator) lir.Query
}

var modes = []mode{
	{"bag", false, (*generative.Generator).Query},
	{"ordered", true, (*generative.Generator).OrderedQuery},
}

func TestGenerativeSynthetic(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	for _, m := range modes {
		t.Run(m.name, func(t *testing.T) {
			t.Parallel()
			for i := 0; i < seeds(); i++ {
				seed := int64(i)
				t.Run(strconv.Itoa(i), func(t *testing.T) {
					t.Parallel()
					rng := rand.New(rand.NewSource(seed))
					spec := generative.SynthCatalog(rng)
					db := freshDB(t)
					for _, def := range generative.TableDefs(spec) {
						if _, err := db.CreateTable(ctx, def); err != nil {
							t.Fatalf("create table %q: %v", def.Name, err)
						}
					}
					data := insertData(t, ctx, db, rng, spec)
					q := m.gen(generative.NewGenerator(rng, spec))
					if err := differential.ThreeWay(ctx, db, scanOf(data), q, m.ordered); err != nil {
						t.Fatalf("seed %d: %v", seed, err)
					}
				})
			}
		})
	}
}

func TestGenerativeSchemas(t *testing.T) {
	t.Parallel()
	ctx := context.Background()
	entries, err := os.ReadDir(".")
	if err != nil {
		t.Fatalf("read tests/gen: %v", err)
	}
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		path := filepath.Join(e.Name(), "schema.rad")
		src, err := os.ReadFile(path)
		if os.IsNotExist(err) {
			continue // not a scenario directory
		}
		if err != nil {
			t.Fatalf("read %s: %v", path, err)
		}
		t.Run(e.Name(), func(t *testing.T) {
			t.Parallel()

			// Introspect once to decide whether the generator can drive this
			// schema; the resulting spec then serves every seed.
			probe := freshDB(t)
			if _, err := probe.MigrateFile(ctx, path, src); err != nil {
				t.Fatalf("migrate: %v", err)
			}
			tables, err := probe.Tables(ctx)
			if err != nil {
				t.Fatalf("introspect: %v", err)
			}
			spec, reason := generative.Introspect(tables)
			if reason != "" {
				t.Skipf("outside the generator's reach: %s", reason)
			}

			for _, m := range modes {
				t.Run(m.name, func(t *testing.T) {
					t.Parallel()
					for i := 0; i < seeds(); i++ {
						seed := int64(i)
						t.Run(strconv.Itoa(i), func(t *testing.T) {
							t.Parallel()
							db := freshDB(t)
							if _, err := db.MigrateFile(ctx, path, src); err != nil {
								t.Fatalf("migrate: %v", err)
							}
							rng := rand.New(rand.NewSource(seed))
							data := insertData(t, ctx, db, rng, spec)
							q := m.gen(generative.NewGenerator(rng, spec))
							if err := differential.ThreeWay(ctx, db, scanOf(data), q, m.ordered); err != nil {
								t.Fatalf("seed %d: %v", seed, err)
							}
						})
					}
				})
			}
		})
	}
}

// scanOf feeds the reference interpreter the rows the runner inserted, keyed by
// table.
func scanOf(data map[string][]lir.Row) differential.ScanFunc {
	return func(_ context.Context, tbl catalog.Table) ([]lir.Row, error) {
		return data[tbl.Name], nil
	}
}

// insertData generates the dataset for spec and inserts it in dependency order,
// returning the rows so the oracle reads exactly what was stored.
func insertData(t *testing.T, ctx context.Context, db *frontend.DB, rng *rand.Rand, spec *generative.Catalog) map[string][]lir.Row {
	t.Helper()
	data := generative.GenerateData(rng, spec)
	for _, tbl := range spec.Tables {
		for _, row := range data[tbl.Name] {
			if err := db.Insert(ctx, tbl.Name, row); err != nil {
				t.Fatalf("insert into %q: %v", tbl.Name, err)
			}
		}
	}
	return data
}

func freshDB(t *testing.T) *frontend.DB {
	t.Helper()
	store, err := kvslate.Open("gen-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	return frontend.Open(store)
}
