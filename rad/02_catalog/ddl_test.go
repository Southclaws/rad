// These tests document schema evolution at the catalog layer: what each DDL
// operation changes, what it refuses, and the invariant that renames never
// touch data (rows are keyed by column ID, data keys by table ID).
package catalog_test

import (
	"strings"
	"testing"

	catalog "rad/rad/02_catalog"
)

func TestRenameTableKeepsIdentity(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	created, err := cat.CreateTable(ctx, "public", usersDef())
	if err != nil {
		t.Fatal(err)
	}

	if err := cat.RenameTable(ctx, "public", "users", "people"); err != nil {
		t.Fatal(err)
	}

	if _, ok, _ := cat.GetTable(ctx, "public", "users"); ok {
		t.Fatal("old name still resolves")
	}
	renamed, ok, err := cat.GetTable(ctx, "public", "people")
	if err != nil || !ok {
		t.Fatalf("new name: ok=%v err=%v", ok, err)
	}
	if renamed.ID != created.ID {
		t.Fatalf("rename changed table ID: %q -> %q", created.ID, renamed.ID)
	}

	// The freed name is reusable; the taken name is not.
	if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
		t.Fatalf("freed name not reusable: %v", err)
	}
	if err := cat.RenameTable(ctx, "public", "users", "people"); err == nil {
		t.Fatal("rename onto taken name accepted")
	}
}

func TestDropTable(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
		t.Fatal(err)
	}

	if err := cat.DropTable(ctx, "public", "users"); err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := cat.GetTable(ctx, "public", "users"); ok {
		t.Fatal("dropped table still resolves")
	}
	if err := cat.DropTable(ctx, "public", "users"); err == nil {
		t.Fatal("double drop accepted")
	}
}

func TestAddColumnRules(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	created, err := cat.CreateTable(ctx, "public", usersDef())
	if err != nil {
		t.Fatal(err)
	}

	// Nullable: fine. Literal default: fine.
	tbl, err := cat.AddColumn(ctx, "public", "users", catalog.ColumnDef{Name: "bio", Type: catalog.TypeText, Nullable: true})
	if err != nil {
		t.Fatal(err)
	}
	tbl, err = cat.AddColumn(ctx, "public", "users", catalog.ColumnDef{
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
	_, err = cat.AddColumn(ctx, "public", "users", catalog.ColumnDef{Name: "req", Type: catalog.TypeText})
	if err == nil || !strings.Contains(err.Error(), "nullable or have a literal default") {
		t.Fatalf("got %v", err)
	}
	// Generator defaults would fabricate different values on every read.
	_, err = cat.AddColumn(ctx, "public", "users", catalog.ColumnDef{
		Name: "gen", Type: catalog.TypeText, Default: &catalog.Default{Func: catalog.DefaultUUID},
	})
	if err == nil {
		t.Fatal("generator default on added column accepted")
	}
	// Duplicates rejected.
	if _, err := cat.AddColumn(ctx, "public", "users", catalog.ColumnDef{Name: "bio", Type: catalog.TypeText, Nullable: true}); err == nil {
		t.Fatal("duplicate column accepted")
	}
}

func TestDropColumnGuards(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", catalog.TableDef{
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

	if _, err := cat.DropColumn(ctx, "public", "users", "id"); err == nil {
		t.Fatal("dropped a primary key column")
	}
	if _, err := cat.DropColumn(ctx, "public", "users", "name"); err == nil || !strings.Contains(err.Error(), "index") {
		t.Fatalf("indexed column: %v", err)
	}
	if _, err := cat.DropColumn(ctx, "public", "orders", "user_id"); err == nil || !strings.Contains(err.Error(), "foreign key") {
		t.Fatalf("fk column: %v", err)
	}

	// Unreferenced columns drop fine.
	tbl, err := cat.DropColumn(ctx, "public", "users", "age")
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := tbl.Column("age"); ok {
		t.Fatal("column still present after drop")
	}
}

// Renaming a column rewrites every metadata reference — PK, indexes, FKs —
// while the column keeps its ID (and therefore its stored data).
func TestRenameColumnRewritesReferences(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", catalog.TableDef{
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
	before, _, _ := cat.GetTable(ctx, "public", "events")
	kindBefore, _ := before.Column("kind")

	tbl, err := cat.RenameColumn(ctx, "public", "events", "kind", "category")
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
	idx, _ := tbl.Index("events_kind_idx")
	if idx.Columns[0] != "category" {
		t.Fatalf("index columns not rewritten: %v", idx.Columns)
	}

	if _, err := cat.RenameColumn(ctx, "public", "events", "id", "category"); err == nil {
		t.Fatal("rename onto existing column accepted")
	}
}

func TestAddDropIndex(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
		t.Fatal(err)
	}

	idx, err := cat.AddIndex(ctx, "public", "users", catalog.IndexDef{Name: "users_age_idx", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	if idx.ID == "" {
		t.Fatal("index has no ID")
	}
	tbl, _, _ := cat.GetTable(ctx, "public", "users")
	if _, ok := tbl.Index("users_age_idx"); !ok {
		t.Fatal("index not persisted")
	}

	if _, err := cat.AddIndex(ctx, "public", "users", catalog.IndexDef{Name: "users_age_idx", Columns: []string{"age"}}); err == nil {
		t.Fatal("duplicate index accepted")
	}
	if _, err := cat.AddIndex(ctx, "public", "users", catalog.IndexDef{Name: "bad", Columns: []string{"ghost"}}); err == nil {
		t.Fatal("index on unknown column accepted")
	}

	if err := cat.DropIndex(ctx, "public", "users", "users_age_idx"); err != nil {
		t.Fatal(err)
	}
	tbl, _, _ = cat.GetTable(ctx, "public", "users")
	if _, ok := tbl.Index("users_age_idx"); ok {
		t.Fatal("index still present after drop")
	}
}
