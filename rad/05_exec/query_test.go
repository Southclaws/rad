package exec

import (
	"context"
	"testing"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// seed populates the users/orders tables created by setup.
func seed(t *testing.T, eng *Engine, ctx context.Context) {
	t.Helper()
	rows := []struct {
		table string
		row   lir.Row
	}{
		{"users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice"), "age": lir.Int64(30)}},
		{"users", lir.Row{"id": lir.Int64(2), "name": lir.Text("Bob"), "age": lir.Int64(25)}},
		{"users", lir.Row{"id": lir.Int64(3), "name": lir.Text("Carol")}},
		{"orders", lir.Row{"id": lir.Int64(100), "user_id": lir.Int64(1), "total": lir.Float64(9.99)}},
		{"orders", lir.Row{"id": lir.Int64(101), "user_id": lir.Int64(1), "total": lir.Float64(25.00)}},
		{"orders", lir.Row{"id": lir.Int64(102), "user_id": lir.Int64(2), "total": lir.Float64(3.50)}},
	}
	for _, r := range rows {
		if err := eng.Insert(ctx, r.table, r.row); err != nil {
			t.Fatal(err)
		}
	}
}

func TestQueryFullScan(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.Scan{Table: "users", Alias: "u"}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 3 {
		t.Fatalf("got %d rows, want 3", len(rows))
	}
	// Primary key order, alias-qualified names.
	if !rows[0]["u.name"].Equal(lir.Text("Alice")) || !rows[2]["u.name"].Equal(lir.Text("Carol")) {
		t.Fatalf("unexpected rows %v", rows)
	}
}

func TestQueryIndexScanLookup(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.IndexScan{
		Table:  "users",
		Alias:  "u",
		Index:  "users_name_idx",
		Prefix: lir.Row{"name": lir.Text("Alice")},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || !rows[0]["u.id"].Equal(lir.Int64(1)) {
		t.Fatalf("got %v", rows)
	}
}

func TestQueryFilterAndProject(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.Project{
		Input: lir.Filter{
			Input: lir.Scan{Table: "orders", Alias: "o"},
			Expr: lir.Eq{
				Left:  lir.ColRef{Alias: "o", Column: "user_id"},
				Right: lir.Literal{Value: lir.Int64(1)},
			},
		},
		Columns: []lir.Projection{
			{Expr: lir.ColRef{Alias: "o", Column: "id"}, As: "order_id"},
			{Expr: lir.ColRef{Alias: "o", Column: "total"}, As: "total"},
		},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 2 {
		t.Fatalf("got %d rows, want 2", len(rows))
	}
	if !rows[0]["order_id"].Equal(lir.Int64(100)) || !rows[0]["total"].Equal(lir.Float64(9.99)) {
		t.Fatalf("unexpected first row %v", rows[0])
	}
	if len(rows[0]) != 2 {
		t.Fatalf("projection should produce exactly 2 columns, got %v", rows[0])
	}
}

func TestQueryNestedLoopJoin(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.Join{
		Left:  lir.Scan{Table: "users", Alias: "u"},
		Right: lir.IndexScan{Table: "orders", Alias: "o", Index: "orders_user_id_idx"},
		Type:  lir.InnerJoin,
		On: lir.Eq{
			Left:  lir.ColRef{Alias: "u", Column: "id"},
			Right: lir.ColRef{Alias: "o", Column: "user_id"},
		},
	}})
	if err != nil {
		t.Fatal(err)
	}
	// Alice has 2 orders, Bob 1, Carol 0.
	if len(rows) != 3 {
		t.Fatalf("got %d rows, want 3", len(rows))
	}
	for _, r := range rows {
		if !r["u.id"].Equal(r["o.user_id"]) {
			t.Fatalf("join predicate violated in %v", r)
		}
	}
}

func TestQueryJoinFilterProjectCombined(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.Project{
		Input: lir.Filter{
			Input: lir.Join{
				Left:  lir.Scan{Table: "users", Alias: "u"},
				Right: lir.Scan{Table: "orders", Alias: "o"},
				Type:  lir.InnerJoin,
				On: lir.Eq{
					Left:  lir.ColRef{Alias: "u", Column: "id"},
					Right: lir.ColRef{Alias: "o", Column: "user_id"},
				},
			},
			Expr: lir.Eq{
				Left:  lir.ColRef{Alias: "u", Column: "name"},
				Right: lir.Literal{Value: lir.Text("Alice")},
			},
		},
		Columns: []lir.Projection{
			{Expr: lir.ColRef{Alias: "u", Column: "name"}, As: "name"},
			{Expr: lir.ColRef{Alias: "o", Column: "total"}, As: "total"},
		},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 2 {
		t.Fatalf("got %d rows, want 2: %v", len(rows), rows)
	}
}

func TestQueryLimit(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.Limit{
		Input: lir.Scan{Table: "orders", Alias: "o"},
		N:     2,
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 2 {
		t.Fatalf("got %d rows, want 2", len(rows))
	}
}

func TestQueryEqWithNullIsFalse(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	// Carol's age is NULL; Eq on NULL must not match.
	rows, err := eng.Query(ctx, lir.Query{Root: lir.Filter{
		Input: lir.Scan{Table: "users", Alias: "u"},
		Expr: lir.Eq{
			Left:  lir.ColRef{Alias: "u", Column: "age"},
			Right: lir.Literal{Value: lir.Null(catalog.TypeInt64)},
		},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 0 {
		t.Fatalf("NULL = NULL matched rows: %v", rows)
	}
}

// Planner validation surfaces before execution.
func TestQueryPlannerErrors(t *testing.T) {
	eng, ctx := setup(t)

	if _, err := eng.Query(ctx, lir.Query{Root: lir.Scan{Table: "nope", Alias: "n"}}); err == nil {
		t.Error("expected unknown-table error")
	}
	if _, err := eng.Query(ctx, lir.Query{Root: lir.Scan{Table: "users"}}); err == nil {
		t.Error("expected missing-alias error")
	}
	if _, err := eng.Query(ctx, lir.Query{Root: lir.IndexScan{Table: "users", Alias: "u", Index: "nope"}}); err == nil {
		t.Error("expected unknown-index error")
	}
}
