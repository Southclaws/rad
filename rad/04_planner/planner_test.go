// These tests document the planner (04): how logical IR nodes lower into
// physical plan nodes, what gets resolved against the catalog on the way
// down (table and index names become catalog objects with IDs), and which
// invalid queries are rejected before execution ever starts.
package planner_test

import (
	"context"
	"strings"
	"testing"

	"rad/rad/01_kv/kvmem"
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	planner "rad/rad/04_planner"
)

// setup provides a catalog with the canonical users/orders schema.
func setup(t *testing.T) (*catalog.Catalog, context.Context) {
	t.Helper()
	ctx := context.Background()
	cat := catalog.New(kvmem.New())
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", catalog.TableDef{
		Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "name", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "users_name_idx", Columns: []string{"name"}}},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", catalog.TableDef{
		Name: "orders",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeInt64},
			{Name: "region", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "orders_region_user_idx", Columns: []string{"region", "user_id"}},
		},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "orders_user_id_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	}); err != nil {
		t.Fatal(err)
	}
	return cat, ctx
}

func plan(t *testing.T, cat *catalog.Catalog, ctx context.Context, root lir.RelNode) planner.Node {
	t.Helper()
	p, err := planner.Plan(ctx, cat, "public", lir.Query{Root: root})
	if err != nil {
		t.Fatal(err)
	}
	return p
}

// A logical Scan lowers to a TableScan carrying the *resolved* table: the
// executor receives catalog IDs, never name strings.
func TestScanLowersToResolvedTableScan(t *testing.T) {
	cat, ctx := setup(t)

	p := plan(t, cat, ctx, lir.Scan{Table: "users", Alias: "u"})

	ts, ok := p.(planner.TableScan)
	if !ok {
		t.Fatalf("got %T, want TableScan", p)
	}
	if ts.Alias != "u" || ts.Table.Name != "users" || ts.Table.ID == "" {
		t.Fatalf("unresolved table scan: %+v", ts)
	}
}

// A logical IndexScan resolves both the table and the index, and the
// equality prefix is validated against the index's column order.
func TestIndexScanLowersResolved(t *testing.T) {
	cat, ctx := setup(t)

	p := plan(t, cat, ctx, lir.IndexScan{
		Table:  "orders",
		Alias:  "o",
		Index:  "orders_region_user_idx",
		Prefix: lir.Row{"region": lir.Text("eu")},
	})

	is, ok := p.(planner.IndexScan)
	if !ok {
		t.Fatalf("got %T, want IndexScan", p)
	}
	if is.Index.ID == "" || is.Index.Name != "orders_region_user_idx" {
		t.Fatalf("unresolved index: %+v", is.Index)
	}
	if is.Table.ID == "" {
		t.Fatalf("unresolved table: %+v", is.Table)
	}
}

// Composite indexes accept any *leading* subset of their columns as an
// equality prefix: (), (region), (region, user_id) — but never a gap.
func TestIndexScanPrefixRules(t *testing.T) {
	cat, ctx := setup(t)

	ok := []lir.Row{
		nil,
		{"region": lir.Text("eu")},
		{"region": lir.Text("eu"), "user_id": lir.Int64(1)},
	}
	for _, prefix := range ok {
		if _, err := planner.Plan(ctx, cat, "public", lir.Query{Root: lir.IndexScan{
			Table: "orders", Alias: "o", Index: "orders_region_user_idx", Prefix: prefix,
		}}); err != nil {
			t.Errorf("prefix %v rejected: %v", prefix, err)
		}
	}

	// user_id without region skips the leading column.
	_, err := planner.Plan(ctx, cat, "public", lir.Query{Root: lir.IndexScan{
		Table: "orders", Alias: "o", Index: "orders_region_user_idx",
		Prefix: lir.Row{"user_id": lir.Int64(1)},
	}})
	if err == nil || !strings.Contains(err.Error(), "leading subset") {
		t.Errorf("non-leading prefix accepted: %v", err)
	}
}

// Composite operators lower structurally: each logical node has exactly one
// physical counterpart today (Join becomes NestedLoopJoin — the only
// strategy so far), and expressions ride along unchanged.
func TestCompositeLowering(t *testing.T) {
	cat, ctx := setup(t)

	p := plan(t, cat, ctx, lir.Limit{
		N: 5,
		Input: lir.Project{
			Columns: []lir.Projection{{Expr: lir.ColRef{Alias: "u", Column: "name"}, As: "name"}},
			Input: lir.Filter{
				Expr: lir.Eq{Left: lir.ColRef{Alias: "u", Column: "id"}, Right: lir.Literal{Value: lir.Int64(1)}},
				Input: lir.Join{
					Left:  lir.Scan{Table: "users", Alias: "u"},
					Right: lir.Scan{Table: "orders", Alias: "o"},
					Type:  lir.InnerJoin,
					On:    lir.Eq{Left: lir.ColRef{Alias: "u", Column: "id"}, Right: lir.ColRef{Alias: "o", Column: "user_id"}},
				},
			},
		},
	})

	limit, ok := p.(planner.Limit)
	if !ok || limit.N != 5 {
		t.Fatalf("root: %T", p)
	}
	project, ok := limit.Input.(planner.Project)
	if !ok || len(project.Columns) != 1 {
		t.Fatalf("limit input: %T", limit.Input)
	}
	filter, ok := project.Input.(planner.Filter)
	if !ok {
		t.Fatalf("project input: %T", project.Input)
	}
	join, ok := filter.Input.(planner.NestedLoopJoin)
	if !ok {
		t.Fatalf("filter input: %T", filter.Input)
	}
	left, ok := join.Left.(planner.TableScan)
	if !ok || left.Table.Name != "users" {
		t.Fatalf("join left: %#v", join.Left)
	}
	right, ok := join.Right.(planner.TableScan)
	if !ok || right.Table.Name != "orders" {
		t.Fatalf("join right: %#v", join.Right)
	}
	if _, ok := join.On.(lir.Eq); !ok {
		t.Fatalf("join predicate lost: %T", join.On)
	}
}

// Planning rejects invalid queries up front, before any KV access — these
// errors reach callers even on an empty store.
func TestPlannerValidation(t *testing.T) {
	cases := []struct {
		name    string
		root    lir.RelNode
		wantErr string
	}{
		{"unknown table", lir.Scan{Table: "ghost", Alias: "g"}, "does not exist"},
		{"scan needs alias", lir.Scan{Table: "users"}, "needs an alias"},
		{"index scan needs alias", lir.IndexScan{Table: "users", Index: "users_name_idx"}, "needs an alias"},
		{"unknown index", lir.IndexScan{Table: "users", Alias: "u", Index: "ghost"}, "no index"},
		{"unsupported join type", lir.Join{
			Left:  lir.Scan{Table: "users", Alias: "u"},
			Right: lir.Scan{Table: "orders", Alias: "o"},
			Type:  "left",
		}, "unsupported join type"},
		{"projection needs As", lir.Project{
			Input:   lir.Scan{Table: "users", Alias: "u"},
			Columns: []lir.Projection{{Expr: lir.ColRef{Alias: "u", Column: "id"}}},
		}, "output name"},
		{"errors propagate from nested nodes", lir.Limit{
			N:     1,
			Input: lir.Filter{Input: lir.Scan{Table: "ghost", Alias: "g"}, Expr: lir.Eq{}},
		}, "does not exist"},
	}

	cat, ctx := setup(t)
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := planner.Plan(ctx, cat, "public", lir.Query{Root: tc.root})
			if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
				t.Fatalf("got %v, want error containing %q", err, tc.wantErr)
			}
		})
	}
}
