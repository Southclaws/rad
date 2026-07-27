package oracle

// The models are oracles, so they are themselves pinned by hand-derived cases —
// not circular: these fix the model's behaviour from first principles, and the
// model then checks the engine.

import (
	"reflect"
	"sort"
	"testing"
)

func stepFrom(adj map[string][]string) func(string) []string {
	return func(s string) []string { return adj[s] }
}

// TestFixpointNew: admit-new yields the reachable set in BFS order and closes
// on cycles and self-loops.
func TestFixpointNew(t *testing.T) {
	cases := []struct {
		name string
		adj  map[string][]string
		want []string
	}{
		{"chain", map[string][]string{"A": {"B"}, "B": {"C"}}, []string{"A", "B", "C"}},
		{"diamond", map[string][]string{"A": {"B", "C"}, "B": {"D"}, "C": {"D"}}, []string{"A", "B", "C", "D"}},
		{"two-cycle", map[string][]string{"A": {"B"}, "B": {"A"}}, []string{"A", "B"}},
		{"self-loop", map[string][]string{"A": {"A"}}, []string{"A"}},
		{"sink", map[string][]string{}, []string{"A"}},
	}
	for _, c := range cases {
		if got := FixpointNew([]string{"A"}, stepFrom(c.adj)); !reflect.DeepEqual(got, c.want) {
			t.Errorf("%s: got %v want %v", c.name, got, c.want)
		}
	}
}

// TestFixpointAll: admit-all keeps one row per path, so a diamond's shared sink
// appears once per derivation.
func TestFixpointAll(t *testing.T) {
	cases := []struct {
		name string
		adj  map[string][]string
		want []string // sorted; multiplicity significant
	}{
		{"chain", map[string][]string{"A": {"B"}, "B": {"C"}}, []string{"A", "B", "C"}},
		{"diamond", map[string][]string{"A": {"B", "C"}, "B": {"D"}, "C": {"D"}}, []string{"A", "B", "C", "D", "D"}},
	}
	for _, c := range cases {
		got := FixpointAll([]string{"A"}, stepFrom(c.adj))
		sort.Strings(got)
		if !reflect.DeepEqual(got, c.want) {
			t.Errorf("%s: got %v want %v", c.name, got, c.want)
		}
	}
}

// TestFixpointNewCompositeState: identity is the whole state value, so a
// composite carrying a nullable tag dedups by (id, tag) — two distinct tags at
// one node stay distinct, and a revisited (id, tag) is admitted once.
func TestFixpointNewCompositeState(t *testing.T) {
	type st struct {
		ID   string
		Tag  string
		Null bool
	}
	step := func(s st) []st {
		switch s.ID {
		case "A":
			return []st{{ID: "B", Null: true}, {ID: "B", Tag: "x"}}
		case "B":
			return []st{{ID: "A", Null: true}} // cycle back with the same anchor state
		}
		return nil
	}
	got := FixpointNew([]st{{ID: "A", Null: true}}, step)
	// (A,NULL), (B,NULL), (B,"x") — the cycle back to (A,NULL) is already seen.
	if len(got) != 3 {
		t.Errorf("got %d states %v, want 3", len(got), got)
	}
}
