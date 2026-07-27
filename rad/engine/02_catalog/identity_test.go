package catalog_test

import (
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func TestDirectCatalogAllocatesLogicalSchemaIDs(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	table, err := cat.CreateTable(ctx, usersDef())
	if err != nil {
		t.Fatal(err)
	}
	if table.SchemaID != 1 {
		t.Fatalf("table schema ID = %d, want 1", table.SchemaID)
	}
	for i, column := range table.Columns {
		want := model.SchemaID(i + 1)
		if column.SchemaID != want {
			t.Fatalf("column %q schema ID = %d, want %d", column.Name, column.SchemaID, want)
		}
	}

	if err := cat.RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	renamed, ok, err := cat.GetTable(ctx, "people")
	if err != nil || !ok {
		t.Fatalf("renamed table: ok=%v err=%v", ok, err)
	}
	if renamed.SchemaID != table.SchemaID || renamed.Columns[1].SchemaID != table.Columns[1].SchemaID {
		t.Fatalf("rename changed logical identity: before=%+v after=%+v", table, renamed)
	}
}

func TestTableSchemaIDsAreNeverReused(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	created, err := cat.CreateTable(ctx, usersDef())
	if err != nil {
		t.Fatal(err)
	}
	if err := cat.DeleteTable(ctx, "users"); err != nil {
		t.Fatal(err)
	}

	replacement := usersDef()
	replacement.ID = created.SchemaID
	replacement.Name = "accounts"
	if _, err := cat.CreateTable(ctx, replacement); err == nil || !strings.Contains(err.Error(), "has already been used") {
		t.Fatalf("reused retired table ID: %v", err)
	}

	replacement.ID = 0
	for i := range replacement.Columns {
		replacement.Columns[i].ID = 0
	}
	next, err := cat.CreateTable(ctx, replacement)
	if err != nil {
		t.Fatal(err)
	}
	if next.SchemaID != created.SchemaID+1 {
		t.Fatalf("next table schema ID = %d, want %d", next.SchemaID, created.SchemaID+1)
	}
}

func TestColumnSchemaIDsAreNeverReusedWithinTable(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	table, err := cat.CreateTable(ctx, usersDef())
	if err != nil {
		t.Fatal(err)
	}
	retired := table.Columns[2].SchemaID
	if _, err := cat.DeleteColumn(ctx, "users", table.Columns[2].Name); err != nil {
		t.Fatal(err)
	}

	if _, err := cat.CreateColumn(ctx, "users", model.ColumnDef{
		ID: retired, Name: "replacement", Type: model.TypeText, Nullable: true,
	}); err == nil || !strings.Contains(err.Error(), "has already been used") {
		t.Fatalf("reused retired column ID: %v", err)
	}

	updated, err := cat.CreateColumn(ctx, "users", model.ColumnDef{
		Name: "replacement", Type: model.TypeText, Nullable: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	created, ok := updated.Column("replacement")
	if !ok {
		t.Fatal("replacement column missing")
	}
	if created.SchemaID != retired+1 {
		t.Fatalf("next column schema ID = %d, want %d", created.SchemaID, retired+1)
	}
}

func TestCreateTableRejectsDuplicateColumnSchemaIDs(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	definition := usersDef()
	definition.ID = 10
	definition.Columns[0].ID = 1
	definition.Columns[1].ID = 1
	if _, err := cat.CreateTable(ctx, definition); err == nil || !strings.Contains(err.Error(), "share schema ID 1") {
		t.Fatalf("duplicate column IDs: %v", err)
	}
	revision, err := cat.Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if revision.Version != 0 {
		t.Fatalf("rejected definition recorded revision %d", revision.Version)
	}
}
