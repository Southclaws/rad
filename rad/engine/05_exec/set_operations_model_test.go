package exec

// An independently-derived model for the bag set operations, in the same
// spirit as the recursive suites' state-space oracles: random small bags run
// through the real engine AND the reference interpreter, both compared
// against a count-map computed directly from the drawn cells. The model
// shares nothing with either implementation — not the canonical row
// identity, not the multiset code — so a mistake shared by engine and
// refexec still fails here.

import (
	"fmt"
	"slices"
	"testing"

	"pgregory.net/rapid"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestSetOperationBagModel(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	// Tiny domains force identity collisions, NULLs included: identity in a
	// set operation treats NULL equal to NULL.
	values := []any{nil, int64(1), int64(2)}
	labels := []any{nil, "x", "y"}
	operations := []string{"concatenate", "intersect_all", "intersect_distinct", "except_all", "except_distinct"}

	rapid.Check(t, func(rt *rapid.T) {
		drawBag := func(name string) [][]any {
			n := rapid.IntRange(0, 5).Draw(rt, name+"_len")
			rows := make([][]any, n)
			for i := range rows {
				rows[i] = []any{
					rapid.SampledFrom(values).Draw(rt, name+"_v"),
					rapid.SampledFrom(labels).Draw(rt, name+"_w"),
				}
			}
			return rows
		}
		bagA, bagB := drawBag("a"), drawBag("b")
		operation := rapid.SampledFrom(operations).Draw(rt, "operation")

		columns := []lir.RowsCol{
			{Name: "v", Kind: lir.KindInt64, Nullable: true},
			{Name: "w", Kind: lir.KindText, Nullable: true},
		}
		left := lir.Rows{Scope: "ra", Columns: columns, Values: bagA}
		right := lir.Rows{Scope: "rb", Columns: columns, Values: bagB}

		var rel lir.Relation
		switch operation {
		case "concatenate":
			rel = lir.Concatenate{Scope: "u", Inputs: []lir.Relation{left, right}}
		case "intersect_all":
			rel = lir.Intersect{Scope: "u", Left: left, Right: right, Quantifier: lir.QuantifierAll}
		case "intersect_distinct":
			rel = lir.Intersect{Scope: "u", Left: left, Right: right, Quantifier: lir.QuantifierDistinct}
		case "except_all":
			rel = lir.Except{Scope: "u", Left: left, Right: right, Quantifier: lir.QuantifierAll}
		default:
			rel = lir.Except{Scope: "u", Left: left, Right: right, Quantifier: lir.QuantifierDistinct}
		}
		q := many(rel)

		expected := bagModel(operation, bagA, bagB)

		engine, err := eng.Execute(ctx, q)
		if err != nil {
			rt.Fatalf("engine: %v", err)
		}
		oracle, err := interpQuery(ctx, eng, q)
		if err != nil {
			rt.Fatalf("refexec: %v", err)
		}
		got := resultKeys(engine)
		ref := resultKeys(oracle)
		if !slices.Equal(got, expected) {
			rt.Fatalf("%s: engine disagrees with the bag model\n a: %v\n b: %v\n engine: %v\n model:  %v", operation, bagA, bagB, got, expected)
		}
		if !slices.Equal(ref, expected) {
			rt.Fatalf("%s: refexec disagrees with the bag model\n a: %v\n b: %v\n refexec: %v\n model:   %v", operation, bagA, bagB, ref, expected)
		}
	})
}

// bagModel computes the expected multiset with plain count maps over the
// drawn cells, returned as a sorted key list for order-free comparison.
func bagModel(operation string, bagA, bagB [][]any) []string {
	key := func(cells []any) string { return fmt.Sprintf("%v|%v", cells[0], cells[1]) }
	countsB := map[string]int{}
	for _, cells := range bagB {
		countsB[key(cells)]++
	}

	var out []string
	emittedOnce := map[string]bool{}
	for _, cells := range bagA {
		k := key(cells)
		switch operation {
		case "concatenate":
			out = append(out, k)
		case "intersect_all":
			if countsB[k] > 0 {
				countsB[k]--
				out = append(out, k)
			}
		case "intersect_distinct":
			if countsB[k] > 0 && !emittedOnce[k] {
				emittedOnce[k] = true
				out = append(out, k)
			}
		case "except_all":
			if countsB[k] > 0 {
				countsB[k]--
			} else {
				out = append(out, k)
			}
		default: // except_distinct
			if countsB[k] == 0 && !emittedOnce[k] {
				emittedOnce[k] = true
				out = append(out, k)
			}
		}
	}
	if operation == "concatenate" {
		for _, cells := range bagB {
			out = append(out, key(cells))
		}
	}
	slices.Sort(out)
	return out
}

// resultKeys renders a many-result's rows in the model's key form, sorted.
func resultKeys(d lir.Datum) []string {
	rows, _ := jsonish(d).([]any)
	out := make([]string, 0, len(rows))
	for _, row := range rows {
		m, _ := row.(map[string]any)
		out = append(out, fmt.Sprintf("%v|%v", m["v"], m["w"]))
	}
	slices.Sort(out)
	return out
}
