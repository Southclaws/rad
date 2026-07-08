package planner_test

// Access-path selection for shaped reads: the IR names no indexes; the
// planner extracts equality predicates and picks PK lookup > longest index
// prefix > full scan.

import (
	"testing"

	lir "rad/rad/03_lir"
	planner "rad/rad/04_planner"
)

func col(c string) lir.Expr             { return lir.ColRef{Column: c} }
func lit(v lir.Value) lir.Expr          { return lir.Literal{Value: v} }
func eq(c string, v lir.Value) lir.Expr { return lir.Eq{Left: col(c), Right: lit(v)} }

func TestChooseAccessPath(t *testing.T) {
	cat, ctx := setup(t) // users(id pk, name idx) + orders(region+user_id idx)

	cases := []struct {
		name   string
		read   lir.Read
		kind   planner.AccessKind
		detail func(t *testing.T, a planner.Access)
	}{
		{
			name: "no filter is a full scan",
			read: lir.Read{Table: "users"},
			kind: planner.AccessFullScan,
		},
		{
			name: "complete pk equality is a point lookup",
			read: lir.Read{Table: "users", Filter: eq("id", lir.Int64(1))},
			kind: planner.AccessPKLookup,
		},
		{
			name: "indexed equality is an index scan",
			read: lir.Read{Table: "users", Filter: eq("name", lir.Text("ada"))},
			kind: planner.AccessIndexScan,
			detail: func(t *testing.T, a planner.Access) {
				if a.Index.Name != "users_name_idx" || len(a.Prefix) != 1 {
					t.Errorf("access = %+v", a)
				}
			},
		},
		{
			name: "longest leading prefix wins on composite indexes",
			read: lir.Read{Table: "orders", Filter: lir.And{Exprs: []lir.Expr{
				eq("region", lir.Text("eu")),
				eq("user_id", lir.Int64(7)),
			}}},
			kind: planner.AccessIndexScan,
			detail: func(t *testing.T, a planner.Access) {
				if a.Index.Name != "orders_region_user_idx" || len(a.Prefix) != 2 {
					t.Errorf("access = %+v", a)
				}
			},
		},
		{
			name: "non-leading equality cannot use the index",
			read: lir.Read{Table: "orders", Filter: eq("user_id", lir.Int64(7))},
			kind: planner.AccessFullScan,
		},
		{
			name: "inequalities do not drive access paths",
			read: lir.Read{Table: "users", Filter: lir.Cmp{Op: lir.OpGt, Left: col("name"), Right: lit(lir.Text("a"))}},
			kind: planner.AccessFullScan,
		},
		{
			name: "OR branches cannot be used as equalities",
			read: lir.Read{Table: "users", Filter: lir.Or{Exprs: []lir.Expr{
				eq("id", lir.Int64(1)),
				eq("id", lir.Int64(2)),
			}}},
			kind: planner.AccessFullScan,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			plan, err := planner.PlanRead(ctx, cat, tc.read)
			if err != nil {
				t.Fatal(err)
			}
			if plan.Access.Kind != tc.kind {
				t.Fatalf("access = %+v, want %s", plan.Access, tc.kind)
			}
			if tc.detail != nil {
				tc.detail(t, plan.Access)
			}
		})
	}
}

// Children includes resolve the referencing table's FK and pick a covering
// index for the child fetch when one exists.
func TestPlanReadIncludes(t *testing.T) {
	cat, ctx := setup(t)

	plan, err := planner.PlanRead(ctx, cat, lir.Read{
		Table: "users",
		Include: []lir.Include{
			{FK: "orders_user_id_fk", Dir: lir.ToChildren, As: "orders"},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	inc := plan.Include[0]
	if inc.Child.Name != "orders" || inc.FK.Name != "orders_user_id_fk" {
		t.Fatalf("include = %+v", inc)
	}
	// orders has no index leading with user_id (its index is region-first),
	// so the child fetch falls back to a scan.
	if inc.ChildIndex != nil {
		t.Fatalf("expected scan fallback, got index %+v", inc.ChildIndex)
	}
}
