// These tests document the catalog layer (02): schema and table definition,
// ID assignment, name resolution, validation rules, and persistence. The
// catalog is plain data in the KV store — everything here runs on the pure-Go
// kvmem backend.
package catalog_test

import (
	"context"
	"strings"
	"testing"

	"rad/rad/01_kv/kvmem"
	catalog "rad/rad/02_catalog"
)

// usersDef is a representative table definition exercising every feature:
// typed columns, nullability, a primary key, and a secondary index.
func usersDef() catalog.TableDef {
	return catalog.TableDef{
		Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "name", Type: catalog.TypeText},
			{Name: "age", Type: catalog.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "users_name_idx", Columns: []string{"name"}}},
	}
}

func newCatalog(t *testing.T) (*catalog.Catalog, *kvmem.Store, context.Context) {
	t.Helper()
	store := kvmem.New()
	return catalog.New(store), store, context.Background()
}

// Schemas are namespaces for tables. Creating one persists it and makes it
// resolvable by name; names are unique.
func TestSchemaLifecycle(t *testing.T) {
	cat, _, ctx := newCatalog(t)

	s, err := cat.CreateSchema(ctx, "public")
	if err != nil {
		t.Fatal(err)
	}
	if s.Name != "public" || s.ID == "" {
		t.Fatalf("unexpected schema %+v", s)
	}

	got, ok, err := cat.GetSchema(ctx, "public")
	if err != nil || !ok || got.ID != s.ID {
		t.Fatalf("GetSchema: %+v ok=%v err=%v", got, ok, err)
	}

	if _, ok, _ := cat.GetSchema(ctx, "nope"); ok {
		t.Fatal("unknown schema resolved")
	}

	if _, err := cat.CreateSchema(ctx, "public"); err == nil {
		t.Fatal("duplicate schema name accepted")
	}
	if _, err := cat.CreateSchema(ctx, ""); err == nil {
		t.Fatal("empty schema name accepted")
	}
}

// CreateTable assigns catalog-wide unique IDs to the table and each column
// and index. IDs, not names, appear in data keys — renames stay cheap.
func TestCreateTableAssignsIDs(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}

	tbl, err := cat.CreateTable(ctx, "public", usersDef())
	if err != nil {
		t.Fatal(err)
	}

	if !strings.HasPrefix(tbl.ID, "t") {
		t.Errorf("table ID %q should have prefix t", tbl.ID)
	}
	seen := map[string]bool{tbl.ID: true}
	for _, c := range tbl.Columns {
		if !strings.HasPrefix(c.ID, "c") {
			t.Errorf("column ID %q should have prefix c", c.ID)
		}
		if seen[c.ID] {
			t.Errorf("duplicate ID %q", c.ID)
		}
		seen[c.ID] = true
	}
	for _, idx := range tbl.Indexes {
		if !strings.HasPrefix(idx.ID, "i") {
			t.Errorf("index ID %q should have prefix i", idx.ID)
		}
		if seen[idx.ID] {
			t.Errorf("duplicate ID %q", idx.ID)
		}
		seen[idx.ID] = true
	}

	// The definition round-trips: columns keep order, types, nullability.
	if len(tbl.Columns) != 3 || tbl.Columns[0].Name != "id" || tbl.Columns[2].Nullable != true {
		t.Fatalf("columns did not round-trip: %+v", tbl.Columns)
	}
	if len(tbl.PrimaryKey) != 1 || tbl.PrimaryKey[0] != "id" {
		t.Fatalf("primary key did not round-trip: %v", tbl.PrimaryKey)
	}
}

// Tables resolve by (schema, name) and by ID; the two paths agree.
func TestTableLookup(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	created, err := cat.CreateTable(ctx, "public", usersDef())
	if err != nil {
		t.Fatal(err)
	}

	byName, ok, err := cat.GetTable(ctx, "public", "users")
	if err != nil || !ok {
		t.Fatalf("GetTable: ok=%v err=%v", ok, err)
	}
	byID, ok, err := cat.GetTableByID(ctx, created.ID)
	if err != nil || !ok {
		t.Fatalf("GetTableByID: ok=%v err=%v", ok, err)
	}
	if byName.ID != byID.ID || byName.Name != byID.Name {
		t.Fatalf("lookups disagree: %+v vs %+v", byName, byID)
	}

	if _, ok, _ := cat.GetTable(ctx, "public", "nope"); ok {
		t.Fatal("unknown table resolved")
	}
	if _, ok, _ := cat.GetTable(ctx, "other", "users"); ok {
		t.Fatal("table resolved in wrong schema")
	}
}

// The helper accessors resolve columns and indexes by name.
func TestTableAccessors(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	tbl, err := cat.CreateTable(ctx, "public", usersDef())
	if err != nil {
		t.Fatal(err)
	}

	col, ok := tbl.Column("age")
	if !ok || col.Type != catalog.TypeInt64 || !col.Nullable {
		t.Fatalf("Column(age) = %+v ok=%v", col, ok)
	}
	if _, ok := tbl.Column("nope"); ok {
		t.Fatal("unknown column resolved")
	}

	idx, ok := tbl.Index("users_name_idx")
	if !ok || len(idx.Columns) != 1 || idx.Columns[0] != "name" {
		t.Fatalf("Index() = %+v ok=%v", idx, ok)
	}
	if _, ok := tbl.Index("nope"); ok {
		t.Fatal("unknown index resolved")
	}
}

// Foreign keys are resolved at definition time: the referenced table must
// exist and the reference must cover its full primary key with matching
// types. The stored FK carries the referenced table's ID.
func TestCreateTableForeignKeys(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	users, err := cat.CreateTable(ctx, "public", usersDef())
	if err != nil {
		t.Fatal(err)
	}

	orders, err := cat.CreateTable(ctx, "public", catalog.TableDef{
		Name: "orders",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeInt64},
		},
		PrimaryKey:  []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{{Name: "orders_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(orders.ForeignKeys) != 1 {
		t.Fatalf("foreign keys: %+v", orders.ForeignKeys)
	}
	if fk := orders.ForeignKeys[0]; fk.RefTableID != users.ID {
		t.Fatalf("FK references %q, want users table ID %q", fk.RefTableID, users.ID)
	}
}

// The catalog is a stateless view over the KV store: a second Catalog on the
// same store sees everything the first one created.
func TestPersistenceAcrossInstances(t *testing.T) {
	cat, store, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
		t.Fatal(err)
	}

	reopened := catalog.New(store)
	tbl, ok, err := reopened.GetTable(ctx, "public", "users")
	if err != nil || !ok {
		t.Fatalf("reopened catalog: ok=%v err=%v", ok, err)
	}
	if len(tbl.Columns) != 3 {
		t.Fatalf("reopened table lost columns: %+v", tbl)
	}
}

// Every validation rule CreateTable enforces, as executable documentation.
// Definitions are rejected atomically: a failed CreateTable leaves nothing
// behind (its DDL transaction rolls back), so the same name can be retried.
func TestCreateTableValidation(t *testing.T) {
	cases := []struct {
		name    string
		def     catalog.TableDef
		wantErr string
	}{
		{
			name:    "table name required",
			def:     catalog.TableDef{Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}}, PrimaryKey: []string{"id"}},
			wantErr: "name is required",
		},
		{
			name: "duplicate column",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{
				{Name: "id", Type: catalog.TypeInt64}, {Name: "id", Type: catalog.TypeText},
			}, PrimaryKey: []string{"id"}},
			wantErr: "duplicate column",
		},
		{
			name:    "unsupported column type",
			def:     catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: "uuid"}}, PrimaryKey: []string{"id"}},
			wantErr: "unsupported type",
		},
		{
			name:    "primary key required",
			def:     catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}}},
			wantErr: "needs a primary key",
		},
		{
			name:    "primary key column must exist",
			def:     catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}}, PrimaryKey: []string{"nope"}},
			wantErr: "does not exist",
		},
		{
			name: "primary key column must not be nullable",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{
				{Name: "id", Type: catalog.TypeInt64, Nullable: true},
			}, PrimaryKey: []string{"id"}},
			wantErr: "must not be nullable",
		},
		{
			name: "index needs columns",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}},
				PrimaryKey: []string{"id"}, Indexes: []catalog.IndexDef{{Name: "empty"}}},
			wantErr: "no columns",
		},
		{
			name: "index column must exist",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}},
				PrimaryKey: []string{"id"}, Indexes: []catalog.IndexDef{{Name: "bad", Columns: []string{"nope"}}}},
			wantErr: "unknown column",
		},
		{
			name: "fk referenced table must exist",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}},
				PrimaryKey:  []string{"id"},
				ForeignKeys: []catalog.ForeignKeyDef{{Name: "fk", Columns: []string{"id"}, RefTable: "ghost", RefColumns: []string{"id"}}}},
			wantErr: "unknown table",
		},
		{
			name: "fk must reference the full primary key",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}},
				PrimaryKey:  []string{"id"},
				ForeignKeys: []catalog.ForeignKeyDef{{Name: "fk", Columns: []string{"id"}, RefTable: "users", RefColumns: []string{"name"}}}},
			wantErr: "primary key",
		},
		{
			name: "fk column types must match",
			def: catalog.TableDef{Name: "x", Columns: []catalog.ColumnDef{
				{Name: "id", Type: catalog.TypeInt64}, {Name: "user_id", Type: catalog.TypeText},
			},
				PrimaryKey:  []string{"id"},
				ForeignKeys: []catalog.ForeignKeyDef{{Name: "fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}}}},
			wantErr: "type mismatch",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cat, _, ctx := newCatalog(t)
			if _, err := cat.CreateSchema(ctx, "public"); err != nil {
				t.Fatal(err)
			}
			if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
				t.Fatal(err)
			}

			_, err := cat.CreateTable(ctx, "public", tc.def)
			if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
				t.Fatalf("got %v, want error containing %q", err, tc.wantErr)
			}

			// Rejection is atomic: nothing with that name was left behind.
			if tc.def.Name != "" {
				if _, ok, _ := cat.GetTable(ctx, "public", tc.def.Name); ok {
					t.Fatalf("rejected table %q exists", tc.def.Name)
				}
			}
		})
	}
}

// Table names are unique per schema, and tables cannot be created in schemas
// that do not exist.
func TestCreateTablePreconditions(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	if _, err := cat.CreateSchema(ctx, "public"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(ctx, "public", usersDef()); err != nil {
		t.Fatal(err)
	}

	if _, err := cat.CreateTable(ctx, "public", usersDef()); err == nil {
		t.Fatal("duplicate table name accepted")
	}
	if _, err := cat.CreateTable(ctx, "ghost", usersDef()); err == nil {
		t.Fatal("table created in nonexistent schema")
	}
}
