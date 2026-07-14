package e2e

// The fixture-driven end-to-end runner. Every subdirectory of tests/e2e is
// one self-contained scenario — a schema, seed data, a PIR program, and
// assertions — described entirely in data (see README.md). This file is the
// only Go here: it discovers each fixture, gives it a fresh in-memory
// database, and runs it in parallel through the real client → server →
// bind → plan → execute path.
//
// A fixture is data, not code, so the conformance suite grows by adding
// directories, never by touching this runner.

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/tests/harness"
)

// fixture is one scenario's test_<name>.json.
type fixture struct {
	// Program is the PIR program to run, wire-format JSON.
	Program json.RawMessage `json:"program"`
	// Result, when present, is the exact ProgramResult.Result the program
	// must produce (shaped by the result statement's cardinality).
	Result json.RawMessage `json:"result"`
	// Error, when present, asserts the program fails and the problem detail
	// contains this substring — the negative-test path.
	Error string `json:"error"`
	// Assertions run after the program and pin the state it left behind.
	Assertions []assertion `json:"assertions"`
}

// assertion is one post-program query and its expected datum.
type assertion struct {
	Name   string          `json:"name"`
	Query  json.RawMessage `json:"query"`
	Expect json.RawMessage `json:"expect"`
}

// seedGroup is one {table, rows} entry of seed.json.
type seedGroup struct {
	Table string            `json:"table"`
	Rows  []json.RawMessage `json:"rows"`
}

func TestE2E(t *testing.T) {
	entries, err := os.ReadDir(".")
	if err != nil {
		t.Fatalf("read tests/e2e: %v", err)
	}
	ran := 0
	for _, e := range entries {
		if !e.IsDir() {
			continue
		}
		dir := e.Name()
		testFile, ok := findTestFile(t, dir)
		if !ok {
			continue // not a fixture directory
		}
		ran++
		t.Run(dir, func(t *testing.T) {
			t.Parallel()
			runFixture(t, dir, testFile)
		})
	}
	if ran == 0 {
		t.Fatal("no fixtures found under tests/e2e")
	}
}

// findTestFile returns the single test_*.json in dir, or ok=false when the
// directory holds no fixture. More than one is a fixture-authoring error.
func findTestFile(t *testing.T, dir string) (string, bool) {
	matches, err := filepath.Glob(filepath.Join(dir, "test_*.json"))
	if err != nil {
		t.Fatalf("%s: glob: %v", dir, err)
	}
	switch len(matches) {
	case 0:
		return "", false
	case 1:
		return matches[0], true
	default:
		t.Fatalf("%s: %d test_*.json files, want exactly one", dir, len(matches))
		return "", false
	}
}

func runFixture(t *testing.T, dir, testFile string) {
	ctx := context.Background()
	d := harness.New(t)

	// 1. Migrate the fixture's schema.
	schemaSrc, err := os.ReadFile(filepath.Join(dir, "schema.rad"))
	if err != nil {
		t.Fatalf("read schema.rad: %v", err)
	}
	if _, err := d.Client.Migrate(ctx, string(schemaSrc)); err != nil {
		t.Fatalf("migrate: %v", err)
	}

	// 2. Seed, in array (FK-dependency) order. seed.json is optional.
	seed(t, ctx, d, dir)

	// 3. Load and run the program.
	var fx fixture
	decodeFile(t, testFile, &fx)

	prog, err := protocol.UnmarshalProgram(fx.Program)
	if err != nil {
		t.Fatalf("decode program: %v", err)
	}
	res, err := d.Client.Execute(ctx, prog)

	if fx.Error != "" {
		if err == nil {
			t.Fatalf("expected program to fail with %q, but it succeeded", fx.Error)
		}
		if !strings.Contains(err.Error(), fx.Error) {
			t.Fatalf("error = %q, want it to contain %q", err, fx.Error)
		}
		return // negative fixtures assert only the failure
	}
	if err != nil {
		t.Fatalf("execute program: %v", err)
	}

	// 4. Diff the program result, when the fixture pins one.
	if len(fx.Result) > 0 {
		if got, want := canonical(t, res.Result), canonicalRaw(t, fx.Result); got != want {
			t.Fatalf("program result mismatch\n got: %s\nwant: %s", got, want)
		}
	}

	// 5. Run each assertion query and diff its datum.
	for _, a := range fx.Assertions {
		q, err := protocol.UnmarshalQuery(a.Query)
		if err != nil {
			t.Fatalf("assertion %q: decode query: %v", a.Name, err)
		}
		datum, err := d.Client.QueryDatum(ctx, q)
		if err != nil {
			t.Fatalf("assertion %q: query: %v", a.Name, err)
		}
		if got, want := canonical(t, datum), canonicalRaw(t, a.Expect); got != want {
			t.Fatalf("assertion %q mismatch\n got: %s\nwant: %s", a.Name, got, want)
		}
	}
}

// seed inserts seed.json's groups through the client, exactly as an
// application's writes would coerce. Absent seed.json means an empty start.
func seed(t *testing.T, ctx context.Context, d *harness.DB, dir string) {
	raw, err := os.ReadFile(filepath.Join(dir, "seed.json"))
	if os.IsNotExist(err) {
		return
	}
	if err != nil {
		t.Fatalf("read seed.json: %v", err)
	}
	var groups []seedGroup
	if err := decode(raw, &groups); err != nil {
		t.Fatalf("decode seed.json: %v", err)
	}
	for _, g := range groups {
		rows := make([]harness.Row, len(g.Rows))
		for i, r := range g.Rows {
			row := harness.Row{}
			if err := decode(r, &row); err != nil {
				t.Fatalf("seed %q row %d: %v", g.Table, i, err)
			}
			rows[i] = row
		}
		d.Insert(g.Table, rows...)
	}
}

// canonical renders a decoded Go value (numbers as json.Number) as canonical
// JSON: json.Marshal sorts object keys, so two structurally equal values
// serialise identically regardless of key order.
func canonical(t *testing.T, v any) string {
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("canonicalise: %v", err)
	}
	return string(b)
}

// canonicalRaw normalises raw fixture JSON the same way, decoding numbers as
// json.Number so a fixture's 1000 matches an int64 column's 1000 exactly.
func canonicalRaw(t *testing.T, raw json.RawMessage) string {
	var v any
	if err := decode(raw, &v); err != nil {
		t.Fatalf("decode expected JSON: %v", err)
	}
	return canonical(t, v)
}

// decode unmarshals with json.Number so integers keep full precision.
func decode(raw []byte, into any) error {
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	return dec.Decode(into)
}

func decodeFile(t *testing.T, path string, into any) {
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	if err := decode(raw, into); err != nil {
		t.Fatalf("decode %s: %v", path, err)
	}
}
