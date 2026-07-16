package generative

import (
	"math/rand"
	"sort"
	"testing"
)

// TestGeneratorCoverage audits the generator's reach: it generates many queries
// (pure generation, no engine) and tallies which constructs and compositions
// appear, so a suite that technically supports a construct but almost never
// reaches it can't fool us. It prints the distribution and fails if a construct
// the generator is meant to emit drops below a floor — a regression guard on
// the generator itself.
func TestGeneratorCoverage(t *testing.T) {
	const n = 2000
	counts := map[string]int{}
	for i := 0; i < n; i++ {
		rng := rand.New(rand.NewSource(int64(i)))
		spec := SynthCatalog(rng)
		g := NewGenerator(rng, spec)
		for f := range Features(g.Query()) {
			counts[f]++
		}
	}

	keys := make([]string, 0, len(counts))
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	t.Logf("generator feature coverage over %d seeds:", n)
	for _, k := range keys {
		t.Logf("  %-22s %5d", k, counts[k])
	}

	// Constructs the generator is meant to emit regularly.
	mustHit := []string{
		"scan", "filter", "project", "order",
		"join_inner", "join_left", "aggregate", "group_by", "global_aggregate",
		"exists", "first", "scalar", "array",
		"arithmetic", "is_null", "and_or", "not",
		"crossing", "correlated_aggregate", "crossing_over_join", "ref_binding",
	}
	const floor = 15
	for _, f := range mustHit {
		if counts[f] < floor {
			t.Errorf("under-explored: %q generated %d/%d times, want ≥ %d", f, counts[f], n, floor)
		}
	}
	// Constructs the generator does not emit, asserted absent so the audit fails
	// loudly the day one starts appearing (promote it into mustHit): it builds no
	// literal `rows` relations, no `cast`, no `slice`, and no crossing nested
	// inside another crossing.
	for _, f := range []string{
		"rows", "cast", "slice", "nested_crossing",
	} {
		if counts[f] != 0 {
			t.Errorf("gap %q now appears (%d) — promote it into mustHit", f, counts[f])
		}
	}
}
