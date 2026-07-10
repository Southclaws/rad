package exec

// Update and Delete semantics: index maintenance, constraint re-checks, and
// delete-restrict through foreign keys (index-assisted and full-scan).

import (
	"strings"
	"testing"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

func TestUpdateRewritesIndexes(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(1), "name": qir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}

	_, found, err := eng.Update(ctx, "users", qir.Row{"id": qir.Int64(1)}, qir.Row{"name": qir.Text("Alicia")})
	if err != nil || !found {
		t.Fatalf("found=%v err=%v", found, err)
	}

	row, _, _ := eng.GetByPrimaryKey(ctx, "users", qir.Row{"id": qir.Int64(1)})
	if !row["name"].Equal(qir.Text("Alicia")) {
		t.Fatalf("update lost: %v", row)
	}
	// The old index entry is gone, the new one serves.
	if rows, _ := eng.ScanIndex(ctx, "users", "users_name_idx", qir.Row{"name": qir.Text("Alice")}); len(rows) != 0 {
		t.Fatalf("stale index entry: %v", rows)
	}
	if rows, _ := eng.ScanIndex(ctx, "users", "users_name_idx", qir.Row{"name": qir.Text("Alicia")}); len(rows) != 1 {
		t.Fatalf("new index entry missing")
	}
}

func TestUpdateValidation(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(1), "name": qir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}

	if _, _, err := eng.Update(ctx, "users", qir.Row{"id": qir.Int64(1)}, qir.Row{"id": qir.Int64(9)}); err == nil {
		t.Error("primary key update accepted")
	}
	if _, _, err := eng.Update(ctx, "users", qir.Row{"id": qir.Int64(1)}, qir.Row{"ghost": qir.Int64(1)}); err == nil {
		t.Error("unknown column accepted")
	}
	if _, _, err := eng.Update(ctx, "users", qir.Row{"id": qir.Int64(1)}, qir.Row{"name": qir.Int64(3)}); err == nil {
		t.Error("type mismatch accepted")
	}
	if _, _, err := eng.Update(ctx, "users", qir.Row{"id": qir.Int64(1)}, qir.Row{"name": qir.Null(catalog.TypeText)}); err == nil {
		t.Error("NULL into non-nullable column accepted")
	}
	// Updating a missing row is found=false, not an error.
	if _, found, err := eng.Update(ctx, "users", qir.Row{"id": qir.Int64(404)}, qir.Row{"name": qir.Text("x")}); err != nil || found {
		t.Errorf("missing row: found=%v err=%v", found, err)
	}
}

func TestUpdateForeignKeyAndUniqueChecks(t *testing.T) {
	eng, ctx := setup(t)
	for i := int64(1); i <= 2; i++ {
		if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(i), "name": qir.Text("u")}); err != nil {
			t.Fatal(err)
		}
	}
	if err := eng.Insert(ctx, "orders", qir.Row{"id": qir.Int64(10), "user_id": qir.Int64(1)}); err != nil {
		t.Fatal(err)
	}

	// Repointing an FK is validated.
	if _, _, err := eng.Update(ctx, "orders", qir.Row{"id": qir.Int64(10)}, qir.Row{"user_id": qir.Int64(2)}); err != nil {
		t.Fatalf("valid fk repoint: %v", err)
	}
	if _, _, err := eng.Update(ctx, "orders", qir.Row{"id": qir.Int64(10)}, qir.Row{"user_id": qir.Int64(42)}); err == nil {
		t.Error("dangling fk repoint accepted")
	}

	// Unique index checks fire on update too.
	if _, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "handles",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "handle", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "handles_uq", Columns: []string{"handle"}, Unique: true}},
	}); err != nil {
		t.Fatal(err)
	}
	_ = eng.Insert(ctx, "handles", qir.Row{"id": qir.Int64(1), "handle": qir.Text("a")})
	_ = eng.Insert(ctx, "handles", qir.Row{"id": qir.Int64(2), "handle": qir.Text("b")})
	if _, _, err := eng.Update(ctx, "handles", qir.Row{"id": qir.Int64(2)}, qir.Row{"handle": qir.Text("a")}); err == nil {
		t.Error("unique violation via update accepted")
	}
	// A no-op rewrite of the same value is fine (it points at the same PK).
	if _, _, err := eng.Update(ctx, "handles", qir.Row{"id": qir.Int64(2)}, qir.Row{"handle": qir.Text("b")}); err != nil {
		t.Errorf("self-value update rejected: %v", err)
	}
}

func TestDeleteRemovesRowAndIndexEntries(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(1), "name": qir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}

	found, err := eng.Delete(ctx, "users", qir.Row{"id": qir.Int64(1)})
	if err != nil || !found {
		t.Fatalf("found=%v err=%v", found, err)
	}
	if _, ok, _ := eng.GetByPrimaryKey(ctx, "users", qir.Row{"id": qir.Int64(1)}); ok {
		t.Fatal("row still readable")
	}
	if rows, _ := eng.ScanIndex(ctx, "users", "users_name_idx", nil); len(rows) != 0 {
		t.Fatalf("index entries survived delete: %v", rows)
	}
	// Deleting again reports not-found.
	if found, err := eng.Delete(ctx, "users", qir.Row{"id": qir.Int64(1)}); err != nil || found {
		t.Fatalf("second delete: found=%v err=%v", found, err)
	}
}

// Delete-restrict: a referenced row cannot go away — via the FK's index
// when one exists (orders_user_id_idx) and via full scan when none does.
func TestDeleteRestrict(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(1), "name": qir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "orders", qir.Row{"id": qir.Int64(10), "user_id": qir.Int64(1)}); err != nil {
		t.Fatal(err)
	}

	_, err := eng.Delete(ctx, "users", qir.Row{"id": qir.Int64(1)})
	if err == nil || !strings.Contains(err.Error(), "referenced by") {
		t.Fatalf("restricted delete: %v", err)
	}

	// Remove the referencing row; the parent is deletable.
	if _, err := eng.Delete(ctx, "orders", qir.Row{"id": qir.Int64(10)}); err != nil {
		t.Fatal(err)
	}
	if found, err := eng.Delete(ctx, "users", qir.Row{"id": qir.Int64(1)}); err != nil || !found {
		t.Fatalf("delete after unreference: found=%v err=%v", found, err)
	}
}

// A self-referential FK does not block deleting a row that only references
// itself... rows referencing themselves are impossible here, but a parent
// task with children must be restricted while a leaf task is deletable.
func TestDeleteRestrictSelfReferential(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "nodes",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "parent_id", Type: catalog.TypeInt64, Nullable: true},
		},
		PrimaryKey:  []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{{Name: "nodes_parent_fk", Columns: []string{"parent_id"}, RefTable: "nodes", RefColumns: []string{"id"}}},
	}); err != nil {
		t.Fatal(err)
	}
	_ = eng.Insert(ctx, "nodes", qir.Row{"id": qir.Int64(1)})
	_ = eng.Insert(ctx, "nodes", qir.Row{"id": qir.Int64(2), "parent_id": qir.Int64(1)})

	if _, err := eng.Delete(ctx, "nodes", qir.Row{"id": qir.Int64(1)}); err == nil {
		t.Fatal("deleted a node with children")
	}
	if _, err := eng.Delete(ctx, "nodes", qir.Row{"id": qir.Int64(2)}); err != nil {
		t.Fatalf("leaf delete: %v", err)
	}
	if found, err := eng.Delete(ctx, "nodes", qir.Row{"id": qir.Int64(1)}); err != nil || !found {
		t.Fatalf("root delete after leaf: found=%v err=%v", found, err)
	}
}
