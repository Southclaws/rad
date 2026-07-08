package exec

// Scan-path behaviors beyond the basics in engine_test.go: composite index
// prefixes, arity and validation errors, and executor-level expression
// failures. Documents what the storage engine accepts and rejects.

import (
	"strings"
	"testing"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// A composite index scans by any leading-column prefix: full equality, a
// leading subset (range over the rest), or empty (whole index, indexed-value
// order).
func TestCompositeIndexScan(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, "public", catalog.TableDef{
		Name: "events",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "region", Type: catalog.TypeText},
			{Name: "user_id", Type: catalog.TypeInt64},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "events_region_user_idx", Columns: []string{"region", "user_id"}}},
	}); err != nil {
		t.Fatal(err)
	}

	seed := []struct {
		id     int64
		region string
		user   int64
	}{
		{1, "eu", 7}, {2, "eu", 9}, {3, "us", 7}, {4, "us", 9}, {5, "eu", 7},
	}
	for _, s := range seed {
		if err := eng.Insert(ctx, "events", lir.Row{
			"id": lir.Int64(s.id), "region": lir.Text(s.region), "user_id": lir.Int64(s.user),
		}); err != nil {
			t.Fatal(err)
		}
	}

	// Full equality on both columns.
	rows, err := eng.ScanIndex(ctx, "events", "events_region_user_idx",
		lir.Row{"region": lir.Text("eu"), "user_id": lir.Int64(7)})
	if err != nil || len(rows) != 2 {
		t.Fatalf("(eu,7): rows=%d err=%v", len(rows), err)
	}

	// Leading column only: all of eu, any user.
	rows, err = eng.ScanIndex(ctx, "events", "events_region_user_idx",
		lir.Row{"region": lir.Text("eu")})
	if err != nil || len(rows) != 3 {
		t.Fatalf("(eu,*): rows=%d err=%v", len(rows), err)
	}

	// Empty prefix walks the whole index in (region, user_id) order.
	rows, err = eng.ScanIndex(ctx, "events", "events_region_user_idx", nil)
	if err != nil || len(rows) != 5 {
		t.Fatalf("(*): rows=%d err=%v", len(rows), err)
	}
	if !rows[0]["region"].Equal(lir.Text("eu")) || !rows[4]["region"].Equal(lir.Text("us")) {
		t.Fatalf("not in indexed-value order: %v", rows)
	}

	// A prefix that skips the leading column is rejected — it would need a
	// full index walk, which is not what an index scan promises.
	_, err = eng.ScanIndex(ctx, "events", "events_region_user_idx",
		lir.Row{"user_id": lir.Int64(7)})
	if err == nil || !strings.Contains(err.Error(), "leading subset") {
		t.Fatalf("non-leading prefix accepted: %v", err)
	}
}

// GetByPrimaryKey demands exactly the primary key columns — no more, no
// fewer — so a caller can't silently under-specify a composite key.
func TestGetByPrimaryKeyArity(t *testing.T) {
	eng, ctx := setup(t)

	if _, _, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{}); err == nil {
		t.Error("empty key accepted")
	}
	if _, _, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{
		"id": lir.Int64(1), "name": lir.Text("Alice"),
	}); err == nil {
		t.Error("over-specified key accepted")
	}
	if _, _, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"name": lir.Text("Alice")}); err == nil {
		t.Error("wrong column accepted")
	}
}

// Unknown tables and indexes fail with clear errors on the read path too.
func TestScanUnknownObjects(t *testing.T) {
	eng, ctx := setup(t)

	if _, err := eng.ScanTable(ctx, "ghost"); err == nil || !strings.Contains(err.Error(), "does not exist") {
		t.Errorf("ScanTable(ghost): %v", err)
	}
	if _, err := eng.ScanIndex(ctx, "users", "ghost", nil); err == nil || !strings.Contains(err.Error(), "no index") {
		t.Errorf("ScanIndex(ghost): %v", err)
	}
}

// Expression errors surface from execution: referencing a column that no
// input provides fails the query rather than silently matching nothing.
func TestQueryUnknownColumnReference(t *testing.T) {
	eng, ctx := setup(t)
	seed(t, eng, ctx)

	_, err := eng.Query(ctx, lir.Query{Root: lir.Filter{
		Input: lir.Scan{Table: "users", Alias: "u"},
		Expr: lir.Eq{
			Left:  lir.ColRef{Alias: "u", Column: "ghost"},
			Right: lir.Literal{Value: lir.Int64(1)},
		},
	}})
	if err == nil || !strings.Contains(err.Error(), "unknown column") {
		t.Fatalf("got %v, want unknown column error", err)
	}

	// Same for a wrong alias with a real column name.
	_, err = eng.Query(ctx, lir.Query{Root: lir.Filter{
		Input: lir.Scan{Table: "users", Alias: "u"},
		Expr: lir.Eq{
			Left:  lir.ColRef{Alias: "x", Column: "id"},
			Right: lir.Literal{Value: lir.Int64(1)},
		},
	}})
	if err == nil || !strings.Contains(err.Error(), "unknown column") {
		t.Fatalf("got %v, want unknown column error", err)
	}
}

// An empty table scans cleanly to zero rows — no phantom errors.
func TestQueryEmptyTable(t *testing.T) {
	eng, ctx := setup(t)

	rows, err := eng.Query(ctx, lir.Query{Root: lir.Scan{Table: "users", Alias: "u"}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 0 {
		t.Fatalf("empty table returned %d rows", len(rows))
	}
}
