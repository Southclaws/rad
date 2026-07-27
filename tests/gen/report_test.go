package gen

// Reporting and an optional run dump. Every generated case checked through the
// differential bumps a counter; TestMain prints the grand total once the suite
// finishes (the headline number). When RAD_GEN_DUMP names a directory, each
// case's schema, query (wire form), and result are collected and written there
// as JSONL at the end — a gitignored record for eyeballing what the generator
// actually produces. The dump is opt-in and off by default, so a normal run
// pays nothing.

import (
	"encoding/json"
	"fmt"
	"hash/fnv"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	api "github.com/Southclaws/rad/rad/server/api"
)

var casesChecked atomic.Int64

func TestMain(m *testing.M) {
	code := m.Run()
	if n := casesChecked.Load(); n > 0 {
		fmt.Printf("\n[generative] checked %d generated queries through the three-way differential\n", n)
	}
	dumpFlush()
	os.Exit(code)
}

func dumpEnabled() bool { return os.Getenv("RAD_GEN_DUMP") != "" }

func dumpDir() string {
	if d := os.Getenv("RAD_GEN_DUMP"); d != "1" {
		return d
	}
	return "runs"
}

type dumpRecord struct {
	Mode   string          `json:"mode"`
	Schema string          `json:"schema"`
	Query  json.RawMessage `json:"query"`
	Result any             `json:"result"`
}

var (
	dumpMu   sync.Mutex
	dumpRecs []dumpRecord
)

// dumpCase records one generated case for the end-of-run dump. Writing is
// deferred to dumpFlush so the run itself does no fixture I/O.
func dumpCase(mode, schema string, q lir.Query, result any) {
	wire, err := json.Marshal(api.WireQuery(q))
	if err != nil {
		wire = json.RawMessage(fmt.Sprintf("%q", err.Error()))
	}
	dumpMu.Lock()
	dumpRecs = append(dumpRecs, dumpRecord{Mode: mode, Schema: schema, Query: wire, Result: result})
	dumpMu.Unlock()
}

// dumpFlush writes one JSON file per case, named "<mode>_<queryhash>.json" — the
// hash is over the wire query, so identical shapes dedup to one file and names
// are stable across runs. Small per-case files keep an editor responsive where a
// single large JSONL does not. The directory is cleared first so it holds only
// this run's cases (it is gitignored and dump-only).
func dumpFlush() {
	if !dumpEnabled() || len(dumpRecs) == 0 {
		return
	}
	dir := dumpDir()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		fmt.Printf("[generative] dump: %v\n", err)
		return
	}
	old, _ := filepath.Glob(filepath.Join(dir, "*.json"))
	for _, f := range old {
		_ = os.Remove(f)
	}

	names := map[string]bool{}
	for _, r := range dumpRecs {
		h := fnv.New32a()
		_, _ = h.Write(r.Query)
		name := fmt.Sprintf("%s_%08x.json", r.Mode, h.Sum32())
		body, err := json.MarshalIndent(r, "", "  ")
		if err != nil {
			continue
		}
		if err := os.WriteFile(filepath.Join(dir, name), append(body, '\n'), 0o644); err != nil {
			fmt.Printf("[generative] dump: %v\n", err)
			return
		}
		names[name] = true
	}
	fmt.Printf("[generative] wrote %d cases (%d distinct) to %s/\n", len(dumpRecs), len(names), dir)
}
