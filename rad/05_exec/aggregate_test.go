package exec

import (
	"testing"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// aggResult runs a root aggregate read and returns its single scalar record.
func aggResult(t *testing.T, eng *Engine, r lir.Read) lir.Row {
	t.Helper()
	recs, err := eng.Read(t.Context(), r)
	if err != nil {
		t.Fatal(err)
	}
	if len(recs) != 1 {
		t.Fatalf("aggregate returned %d records, want exactly 1", len(recs))
	}
	return recs[0].Columns
}

func TestAggregateRootCountSumAvgMinMax(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	got := aggResult(t, eng, lir.Read{
		Table: "users",
		Aggs: []lir.AggTerm{
			{Fn: lir.AggCount, As: "total"},           // rows
			{Fn: lir.AggCount, Column: "age", As: "n"}, // non-NULL ages
			{Fn: lir.AggSum, Column: "age", As: "sum"},
			{Fn: lir.AggAvg, Column: "age", As: "avg"},
			{Fn: lir.AggMin, Column: "age", As: "min"},
			{Fn: lir.AggMax, Column: "age", As: "max"},
			{Fn: lir.AggMin, Column: "name", As: "first_name"}, // text min
		},
	})

	// Alice 30, Bob 25, Carol NULL.
	assertValue(t, got, "total", lir.Int64(3))
	assertValue(t, got, "n", lir.Int64(2))
	assertValue(t, got, "sum", lir.Int64(55)) // sum(int64) stays int64
	assertValue(t, got, "avg", lir.Float64(27.5))
	assertValue(t, got, "min", lir.Int64(25))
	assertValue(t, got, "max", lir.Int64(30))
	assertValue(t, got, "first_name", lir.Text("Alice"))
}

func TestAggregateSumFloatKeepsType(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	got := aggResult(t, eng, lir.Read{
		Table: "orders",
		Aggs: []lir.AggTerm{
			{Fn: lir.AggSum, Column: "total", As: "revenue"},
			{Fn: lir.AggMax, Column: "total", As: "biggest"},
		},
	})
	// 9.99 + 25.00 + 3.50
	assertValue(t, got, "revenue", lir.Float64(38.49))
	assertValue(t, got, "biggest", lir.Float64(25.00))
}

func TestAggregateFilteredCount(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	// Only user 1's orders.
	got := aggResult(t, eng, lir.Read{
		Table: "orders",
		Filter: lir.Eq{
			Left:  lir.ColRef{Column: "user_id"},
			Right: lir.Literal{Value: lir.Int64(1)},
		},
		Aggs: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	})
	assertValue(t, got, "n", lir.Int64(2))
}

func TestAggregateEmptyInput(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	// A filter that matches nothing.
	got := aggResult(t, eng, lir.Read{
		Table: "users",
		Filter: lir.Eq{
			Left:  lir.ColRef{Column: "name"},
			Right: lir.Literal{Value: lir.Text("Nobody")},
		},
		Aggs: []lir.AggTerm{
			{Fn: lir.AggCount, As: "n"},
			{Fn: lir.AggSum, Column: "age", As: "sum"},
			{Fn: lir.AggAvg, Column: "age", As: "avg"},
			{Fn: lir.AggMin, Column: "age", As: "min"},
		},
	})
	// count of nothing is 0; every other fold of nothing is NULL.
	assertValue(t, got, "n", lir.Int64(0))
	if !got["sum"].Null || got["sum"].Type != catalog.TypeInt64 {
		t.Errorf("sum of empty = %v, want NULL int64", got["sum"])
	}
	if !got["avg"].Null || got["avg"].Type != catalog.TypeFloat64 {
		t.Errorf("avg of empty = %v, want NULL float64", got["avg"])
	}
	if !got["min"].Null {
		t.Errorf("min of empty = %v, want NULL", got["min"])
	}
}

func TestAggregateInclude(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	// Each user with a folded order count and revenue, in one read.
	recs, err := eng.Read(ctx, lir.Read{
		Table:   "users",
		OrderBy: []lir.OrderTerm{{Column: "id"}},
		Include: []lir.Include{{
			FK:  "orders_user_fk",
			Dir: lir.ToChildren,
			As:  "order_stats",
			Aggs: []lir.AggTerm{
				{Fn: lir.AggCount, As: "n"},
				{Fn: lir.AggSum, Column: "total", As: "revenue"},
			},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(recs) != 3 {
		t.Fatalf("got %d users, want 3", len(recs))
	}

	// Alice: 2 orders, 34.99; Bob: 1, 3.50; Carol: 0, NULL revenue.
	assertValue(t, recs[0].Scalars["order_stats"], "n", lir.Int64(2))
	assertValue(t, recs[0].Scalars["order_stats"], "revenue", lir.Float64(34.99))
	assertValue(t, recs[1].Scalars["order_stats"], "n", lir.Int64(1))
	assertValue(t, recs[2].Scalars["order_stats"], "n", lir.Int64(0))
	if rev := recs[2].Scalars["order_stats"]["revenue"]; !rev.Null {
		t.Errorf("Carol revenue = %v, want NULL (no orders)", rev)
	}
	// An aggregate include populates Scalars, not Children.
	if recs[0].Children["order_stats"] != nil {
		t.Errorf("aggregate include should not populate Children")
	}
}

func assertValue(t *testing.T, row lir.Row, key string, want lir.Value) {
	t.Helper()
	got, ok := row[key]
	if !ok {
		t.Errorf("missing key %q in %v", key, row)
		return
	}
	if got.Null != want.Null || got.Type != want.Type {
		t.Errorf("%s = %v, want %v", key, got, want)
		return
	}
	// Float sums accumulate rounding; compare within tolerance.
	if want.Type == catalog.TypeFloat64 {
		if d := got.Float64 - want.Float64; d < -1e-9 || d > 1e-9 {
			t.Errorf("%s = %v, want ≈%v", key, got.Float64, want.Float64)
		}
		return
	}
	if !got.Equal(want) {
		t.Errorf("%s = %v, want %v", key, got, want)
	}
}
