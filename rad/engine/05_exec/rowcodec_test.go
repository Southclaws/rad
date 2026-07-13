package exec

// The row-storage invariant behind cheap migrations: rows are keyed by
// column ID, so schema evolution (rename/add/drop column) never rewrites
// data, and old rows remain readable under the new definition.

import (
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestRenameColumnPreservesData(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}

	if _, err := eng.Catalog().RenameColumn(ctx, "users", "name", "full_name"); err != nil {
		t.Fatal(err)
	}

	row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !row["full_name"].Equal(lir.Text("Alice")) {
		t.Fatalf("data lost across rename: %v", row)
	}
	if _, present := row["name"]; present {
		t.Fatalf("old column name leaked into row: %v", row)
	}

	// Writes under the new name work, and the renamed index still serves.
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(2), "full_name": lir.Text("Bob")}); err != nil {
		t.Fatal(err)
	}
	rows, err := eng.ScanIndex(ctx, "users", "users_name_idx", lir.Row{"full_name": lir.Text("Alice")})
	if err != nil || len(rows) != 1 {
		t.Fatalf("index after rename: rows=%d err=%v", len(rows), err)
	}
}

func TestCreateColumnOldRowsReadDefaults(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}

	if _, err := eng.Catalog().CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "active", Type: catalog.TypeBool, Default: &catalog.Default{Bool: true},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "bio", Type: catalog.TypeText, Nullable: true,
	}); err != nil {
		t.Fatal(err)
	}

	row, _, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil {
		t.Fatal(err)
	}
	if !row["active"].Equal(lir.Bool(true)) {
		t.Fatalf("literal default not served for old row: %v", row["active"])
	}
	if !row["bio"].Null {
		t.Fatalf("added nullable column should read NULL: %v", row["bio"])
	}
}

func TestDeleteColumnHidesData(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice"), "age": lir.Int64(30)}); err != nil {
		t.Fatal(err)
	}

	if _, err := eng.Catalog().DeleteColumn(ctx, "users", "age"); err != nil {
		t.Fatal(err)
	}

	row, _, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil {
		t.Fatal(err)
	}
	if _, present := row["age"]; present {
		t.Fatalf("dropped column still readable: %v", row)
	}
	// The name is reusable with a fresh ID; old data does not bleed in.
	if _, err := eng.Catalog().CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "age", Type: catalog.TypeInt64, Nullable: true,
	}); err != nil {
		t.Fatal(err)
	}
	row, _, _ = eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if !row["age"].Null {
		t.Fatalf("recreated column resurrected old data: %v", row["age"])
	}
}
