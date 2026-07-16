package emit

// The emitted fixture must be a valid, runnable e2e fixture: the schema
// migrates, the query program is well-formed wire, and the files are all
// present. (That the program executes to the pinned result is covered by the
// api WireQuery round-trip test — the program here is exactly WireQuery's
// output.)

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	generative "github.com/Southclaws/rad/rad/engine/05_exec/generative"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/protocol"
	"pgregory.net/rapid"
)

type genCase struct {
	spec *generative.Catalog
	data map[string][]lir.Row
	q    lir.Query
}

func TestFixtureIsValid(t *testing.T) {
	ctx := context.Background()
	gc := rapid.Custom(func(rt *rapid.T) genCase {
		spec := generative.SynthCatalog(rt)
		data := generative.GenerateData(rt, spec)
		return genCase{spec: spec, data: data, q: generative.NewGenerator(rt, spec).Query()}
	}).Example(7)

	fixDir, err := Fixture(ctx, t.TempDir(), Case{Spec: gc.spec, Data: gc.data, Query: gc.q, Mode: "smoke"})
	if err != nil {
		t.Fatalf("emit: %v", err)
	}

	for _, f := range []string{"schema.rad", "seed.json", "BUG.md"} {
		if _, err := os.Stat(filepath.Join(fixDir, f)); err != nil {
			t.Errorf("missing %s: %v", f, err)
		}
	}

	// The query program is well-formed wire — it survives the same validating
	// decode the e2e runner applies.
	matches, _ := filepath.Glob(filepath.Join(fixDir, "test_*.json"))
	if len(matches) != 1 {
		t.Fatalf("want exactly one test_*.json, got %d", len(matches))
	}
	raw, err := os.ReadFile(matches[0])
	if err != nil {
		t.Fatal(err)
	}
	var fx struct {
		Program json.RawMessage `json:"program"`
		Result  json.RawMessage `json:"result"`
	}
	if err := json.Unmarshal(raw, &fx); err != nil {
		t.Fatalf("test file not JSON: %v", err)
	}
	if _, err := protocol.UnmarshalProgram(fx.Program); err != nil {
		t.Fatalf("emitted program is invalid wire: %v", err)
	}
	if len(fx.Result) == 0 {
		t.Error("emitted fixture has no expected result")
	}

	// The schema migrates cleanly into a fresh database.
	schema, err := os.ReadFile(filepath.Join(fixDir, "schema.rad"))
	if err != nil {
		t.Fatal(err)
	}
	store, err := kvslate.Open("emit-smoke", "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if _, err := frontend.Open(store).MigrateFile(ctx, "schema.rad", schema); err != nil {
		t.Fatalf("emitted schema.rad does not migrate: %v", err)
	}
}
