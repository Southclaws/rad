package planner_test

// Aggregate planning is validation: a folded read reuses the record read's
// access path (proven in read_test), so these tests pin down only what the
// planner rejects — the rules the executor is then allowed to assume.

import (
	"strings"
	"testing"

	lir "rad/rad/03_lir"
	planner "rad/rad/04_planner"
)

func TestPlanAggregateReusesAccessPath(t *testing.T) {
	cat, ctx := setup(t)

	// A count filtered by the primary key should still choose a PK lookup:
	// the fold rides whatever access path the filter earns.
	p, err := planner.PlanRead(ctx, cat, lir.Read{
		Table:  "users",
		Filter: eq("id", lir.Int64(1)),
		Aggs:   []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if p.Access.Kind != planner.AccessPKLookup {
		t.Errorf("access = %q, want pk_lookup", p.Access.Kind)
	}
	if len(p.Aggs) != 1 {
		t.Errorf("aggs not carried into plan: %+v", p.Aggs)
	}
}

func TestPlanAggregateRejections(t *testing.T) {
	cat, ctx := setup(t)

	cases := []struct {
		name string
		read lir.Read
		want string
	}{
		{
			name: "sum on a non-numeric column",
			read: lir.Read{Table: "users", Aggs: []lir.AggTerm{
				{Fn: lir.AggSum, Column: "name", As: "x"},
			}},
			want: "numeric",
		},
		{
			name: "avg without a column",
			read: lir.Read{Table: "users", Aggs: []lir.AggTerm{
				{Fn: lir.AggAvg, As: "x"},
			}},
			want: "needs a column",
		},
		{
			name: "count of an unknown column",
			read: lir.Read{Table: "users", Aggs: []lir.AggTerm{
				{Fn: lir.AggCount, Column: "nope", As: "x"},
			}},
			want: "no column",
		},
		{
			name: "missing output name",
			read: lir.Read{Table: "users", Aggs: []lir.AggTerm{
				{Fn: lir.AggCount},
			}},
			want: "output name",
		},
		{
			name: "duplicate output name",
			read: lir.Read{Table: "users", Aggs: []lir.AggTerm{
				{Fn: lir.AggCount, As: "x"},
				{Fn: lir.AggMax, Column: "id", As: "x"},
			}},
			want: "duplicate",
		},
		{
			name: "aggregate can't also order or limit",
			read: lir.Read{Table: "users",
				OrderBy: []lir.OrderTerm{{Column: "id"}},
				Aggs:    []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
			},
			want: "can't also order",
		},
		{
			name: "aggregate can't also include relations",
			read: lir.Read{Table: "users",
				Include: []lir.Include{{FK: "orders_user_id_fk", Dir: lir.ToChildren, As: "o"}},
				Aggs:    []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
			},
			want: "can't also",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			_, err := planner.PlanRead(ctx, cat, c.read)
			if err == nil {
				t.Fatalf("expected error containing %q, got nil", c.want)
			}
			if !strings.Contains(err.Error(), c.want) {
				t.Errorf("error %q does not contain %q", err, c.want)
			}
		})
	}
}

func TestPlanAggregateIncludeRejections(t *testing.T) {
	cat, ctx := setup(t)

	// A parent-direction include cannot aggregate: cardinality is zero-or-one.
	_, err := planner.PlanRead(ctx, cat, lir.Read{
		Table: "orders",
		Include: []lir.Include{{
			FK: "orders_user_id_fk", Dir: lir.ToParent, As: "owner",
			Aggs: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
		}},
	})
	if err == nil || !strings.Contains(err.Error(), "cardinality") {
		t.Fatalf("parent aggregate should be rejected by cardinality, got %v", err)
	}

	// A children aggregate include cannot also order/limit/nest.
	_, err = planner.PlanRead(ctx, cat, lir.Read{
		Table: "users",
		Include: []lir.Include{{
			FK: "orders_user_id_fk", Dir: lir.ToChildren, As: "stats",
			Limit: 5,
			Aggs:  []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
		}},
	})
	if err == nil || !strings.Contains(err.Error(), "can't also") {
		t.Fatalf("children aggregate with limit should be rejected, got %v", err)
	}
}
