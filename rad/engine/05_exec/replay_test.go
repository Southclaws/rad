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
)

func TestReplayDeterminism(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	for name, q := range conformanceQueries() {
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
