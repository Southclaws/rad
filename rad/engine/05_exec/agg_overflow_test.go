package exec

// int64 sum overflow is a data error, not a silent two's-complement wrap — and
// it is decided by the *true* total, not by intermediate accumulation, so the
// result never depends on aggregation order / access path. The differential
// oracle can't catch this on its own (the reference interpreter reimplements
// the fold, so a shared wrap would agree-while-wrong); this enumerated test is
// the independent pin, same role the scalar truth-tables play for eval.

import (
	"math"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestAggregateSumOverflow(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	sum := func(vals ...int64) (lir.Datum, error) {
		rows := make([][]any, len(vals))
		for i, v := range vals {
			rows[i] = []any{v}
		}
		return eng.Execute(ctx, lir.Query{Card: lir.CardScalar, Root: lir.Aggregate{
			Input: lir.Rows{
				Scope:   "r",
				Columns: []lir.RowsCol{{Name: "v", Kind: lir.KindInt64}},
				Values:  rows,
			},
			Terms: []lir.AggTerm{{Fn: lir.AggSum, Arg: qcol("r", "v"), As: "s"}},
		}})
	}

	// A true total outside int64 range fails cleanly, rather than wrapping.
	for _, c := range [][]int64{
		{math.MaxInt64, 1},
		{math.MaxInt64, math.MaxInt64},
		{math.MinInt64, -1},
		{math.MinInt64, math.MinInt64},
	} {
		if _, err := sum(c...); err == nil || !reject.IsRuntime(err) {
			t.Errorf("sum(%v) = %v, want execution_failed overflow", c, err)
		}
	}

	// Order independence: intermediates would overflow, but the true total is
	// in range, so every ordering must succeed with the same value. A naive
	// per-add checked sum would (wrongly) error on the MaxInt64+1 step.
	for _, order := range [][]int64{
		{math.MaxInt64, 1, -1},
		{1, math.MaxInt64, -1},
		{-1, 1, math.MaxInt64},
	} {
		d, err := sum(order...)
		if err != nil {
			t.Errorf("sum(%v) errored: %v (true total %d is in range)", order, err, int64(math.MaxInt64))
			continue
		}
		if d.Kind != lir.DatumScalar || d.Scalar != lir.Int64(math.MaxInt64) {
			t.Errorf("sum(%v) = %v, want %d", order, d, int64(math.MaxInt64))
		}
	}

	// A sum comfortably in range is unaffected.
	if d, err := sum(2, 3, 5); err != nil || d.Scalar != lir.Int64(10) {
		t.Errorf("sum(2,3,5) = %v, %v, want 10", d, err)
	}
}
