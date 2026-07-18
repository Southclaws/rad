// These tests document schema evolution at the catalog layer: what each schema-change
// operation changes, what it refuses, and the invariant that renames never
// touch data (rows are keyed by column ID, data keys by table ID).
package catalog_test

import (
	"strings"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

func TestRenameTableKeepsIdentity(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	created, err := cat.CreateTable(ctx, usersDef())
	if err != nil {
		t.Fatal(err)
	}

	if err := cat.RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}

	if _, ok, _ := cat.GetTable(ctx, "users"); ok {
		t.Fatal("old name still resolves")
	}
	renamed, ok, err := cat.GetTable(ctx, "people")
	if err != nil || !ok {
		t.Fatalf("new name: ok=%v err=%v", ok, err)
	}
	if renamed.ID != created.ID {
		t.Fatalf("rename changed table ID: %q -> %q", created.ID, renamed.ID)
	}

	// The freed name is reusable; the taken name is not.
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatalf("freed name not reusable: %v", err)
	}
	if err := cat.RenameTable(ctx, "users", "people"); err == nil {
		t.Fatal("rename onto taken name accepted")
	}
}

func TestDeleteTable(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatal(err)
	}

	if err := cat.DeleteTable(ctx, "users"); err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := cat.GetTable(ctx, "users"); ok {
		t.Fatal("deleted table still resolves")
	}
	if err := cat.DeleteTable(ctx, "users"); err == nil {
		t.Fatal("double delete accepted")
	}
}

func TestCreateColumnRules(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	created, err := cat.CreateTable(ctx, usersDef())
	if err != nil {
		t.Fatal(err)
	}

	// Nullable: fine. Literal default: fine.
	tbl, err := cat.CreateColumn(ctx, "users", catalog.ColumnDef{Name: "bio", Type: catalog.TypeText, Nullable: true})
	if err != nil {
		t.Fatal(err)
	}
	tbl, err = cat.CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "active", Type: catalog.TypeBool, Default: &catalog.Default{Bool: true},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(tbl.Columns) != len(created.Columns)+2 {
		t.Fatalf("columns = %d", len(tbl.Columns))
	}
	// New columns get fresh IDs.
	bio, _ := tbl.Column("bio")
	active, _ := tbl.Column("active")
	if bio.ID == "" || active.ID == "" || bio.ID == active.ID {
		t.Fatalf("bad column IDs: %q %q", bio.ID, active.ID)
	}

	// Non-nullable without a default: existing rows would have no value.
	_, err = cat.CreateColumn(ctx, "users", catalog.ColumnDef{Name: "req", Type: catalog.TypeText})
	if err == nil || !strings.Contains(err.Error(), "nullable or have a literal default") {
		t.Fatalf("got %v", err)
	}
	// Generator defaults would fabricate different values on every read.
	_, err = cat.CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "gen", Type: catalog.TypeText, Default: &catalog.Default{Func: catalog.DefaultUUID},
	})
	if err == nil {
		t.Fatal("generator default on added column accepted")
	}
	// Duplicates rejected.
	if _, err := cat.CreateColumn(ctx, "users", catalog.ColumnDef{Name: "bio", Type: catalog.TypeText, Nullable: true}); err == nil {
		t.Fatal("duplicate column accepted")
	}
}

func TestDeleteColumnGuards(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "orders",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeInt64},
		},
		PrimaryKey:  []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{{Name: "fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}}},
	}); err != nil {
		t.Fatal(err)
	}

	if _, err := cat.DeleteColumn(ctx, "users", "id"); err == nil {
		t.Fatal("deleted a primary key column")
	}
	if _, err := cat.DeleteColumn(ctx, "users", "name"); err == nil || !strings.Contains(err.Error(), "index") {
		t.Fatalf("indexed column: %v", err)
	}
	if _, err := cat.DeleteColumn(ctx, "orders", "user_id"); err == nil || !strings.Contains(err.Error(), "foreign key") {
		t.Fatalf("fk column: %v", err)
	}

	// Unreferenced columns delete fine.
	tbl, err := cat.DeleteColumn(ctx, "users", "age")
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := tbl.Column("age"); ok {
		t.Fatal("column still present after delete")
	}
}

// Renaming a column rewrites every metadata reference — PK, indexes, FKs —
// while the column keeps its ID (and therefore its stored data).
func TestRenameColumnRewritesReferences(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "events",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "kind", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "events_kind_idx", Columns: []string{"kind"}}},
	}); err != nil {
		t.Fatal(err)
	}
	before, _, _ := cat.GetTable(ctx, "events")
	kindBefore, _ := before.Column("kind")

	tbl, err := cat.RenameColumn(ctx, "events", "kind", "category")
	if err != nil {
		t.Fatal(err)
	}

	category, ok := tbl.Column("category")
	if !ok || category.ID != kindBefore.ID {
		t.Fatalf("rename changed column identity: %+v", category)
	}
	if _, ok := tbl.Column("kind"); ok {
		t.Fatal("old column name still present")
	}
	idx, ok := tbl.Index("events_category_idx")
	if !ok {
		t.Fatal("derived index name was not rewritten")
	}
	if idx.Columns[0] != "category" {
		t.Fatalf("index columns not rewritten: %v", idx.Columns)
	}

	if _, err := cat.RenameColumn(ctx, "events", "id", "category"); err == nil {
		t.Fatal("rename onto existing column accepted")
	}
}

func TestRenameColumnRewritesReferencingTables(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "parents",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "children",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "parent_id", Type: catalog.TypeInt64},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{{
			Name: "children_parent_fk", Columns: []string{"parent_id"},
			RefTable: "parents", RefColumns: []string{"id"},
		}},
	}); err != nil {
		t.Fatal(err)
	}

	if _, err := cat.RenameColumn(ctx, "parents", "id", "parent_key"); err != nil {
		t.Fatal(err)
	}
	children, ok, err := cat.GetTable(ctx, "children")
	if err != nil || !ok {
		t.Fatalf("children: ok=%v err=%v", ok, err)
	}
	if got := children.ForeignKeys[0].RefColumns; len(got) != 1 || got[0] != "parent_key" {
		t.Fatalf("referenced columns not rewritten: %v", got)
	}
}

func TestAddDeleteIndex(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatal(err)
	}

	idx, err := cat.CreateIndex(ctx, "users", catalog.IndexDef{Name: "users_age_idx", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	if idx.ID == "" {
		t.Fatal("index has no ID")
	}
	tbl, _, _ := cat.GetTable(ctx, "users")
	if _, ok := tbl.Index("users_age_idx"); !ok {
		t.Fatal("index not persisted")
	}

	if _, err := cat.CreateIndex(ctx, "users", catalog.IndexDef{Name: "users_age_idx", Columns: []string{"age"}}); err == nil {
		t.Fatal("duplicate index accepted")
	}
	if _, err := cat.CreateIndex(ctx, "users", catalog.IndexDef{Name: "bad", Columns: []string{"ghost"}}); err == nil {
		t.Fatal("index on unknown column accepted")
	}

	if err := cat.DeleteIndex(ctx, "users", "users_age_idx"); err != nil {
		t.Fatal(err)
	}
	tbl, _, _ = cat.GetTable(ctx, "users")
	if _, ok := tbl.Index("users_age_idx"); ok {
		t.Fatal("index still present after delete")
	}
}

// A table another table references through a foreign key cannot be deleted
// until the referencing table goes first; a self-reference never blocks its
// own table deletion.
func TestDeleteTableReferencedByForeignKey(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "boards",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "owner_id", Type: catalog.TypeInt64},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "boards_owner_fk", Columns: []string{"owner_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	}); err != nil {
		t.Fatal(err)
	}

	err := cat.DeleteTable(ctx, "users")
	if err == nil {
		t.Fatal("deleted a table another table references")
	}
	if !strings.Contains(err.Error(), "boards_owner_fk") || !strings.Contains(err.Error(), `"boards"`) {
		t.Errorf("error should name the referencing table and foreign key, got: %v", err)
	}
	if _, ok, _ := cat.GetTable(ctx, "users"); !ok {
		t.Fatal("failed delete removed the table anyway")
	}

	// Referencing table first, then the parent.
	if err := cat.DeleteTable(ctx, "boards"); err != nil {
		t.Fatal(err)
	}
	if err := cat.DeleteTable(ctx, "users"); err != nil {
		t.Fatalf("delete after removing the referencing table: %v", err)
	}
}

// A self-referential foreign key dies with its own table.
func TestDeleteTableWithSelfReference(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "employees",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "manager_id", Type: catalog.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "employees_manager_fk", Columns: []string{"manager_id"}, RefTable: "employees", RefColumns: []string{"id"}},
		},
	}); err != nil {
		t.Fatal(err)
	}

	if err := cat.DeleteTable(ctx, "employees"); err != nil {
		t.Fatalf("self-referential table should delete cleanly: %v", err)
	}
}
