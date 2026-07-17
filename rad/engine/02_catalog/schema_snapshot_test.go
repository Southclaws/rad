package catalog_test

import (
	"encoding/json"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestCanonicalSchemaRebuildsCatalogWithoutPhysicalIDs(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	users, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText, Default: &catalog.Default{Func: catalog.DefaultUUID}},
			{Name: "email", Type: catalog.TypeText, Format: "email"},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "users_email_unique", Columns: []string{"email"}, Unique: true},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	orders, err := cat.CreateTable(ctx, catalog.TableDef{
		Name: "orders",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeText},
			{Name: "paid", Type: catalog.TypeBool, Default: &catalog.Default{Bool: false}},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "orders_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	schema, err := cat.Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(schema.Tables) != 2 || schema.Tables[0].Name != "orders" || schema.Tables[1].Name != "users" {
		t.Fatalf("canonical table order = %+v", schema.Tables)
	}
	if schema.Tables[0].ForeignKeys[0].RefTable != "users" {
		t.Fatalf("foreign key did not resolve to a table name: %+v", schema.Tables[0].ForeignKeys)
	}
	raw, err := schema.CanonicalJSON()
	if err != nil {
		t.Fatal(err)
	}
	for _, physicalID := range []string{users.ID, orders.ID, users.Columns[0].ID, orders.ForeignKeys[0].ID} {
		if containsJSONValue(raw, physicalID) {
			t.Fatalf("canonical schema leaked physical ID %q: %s", physicalID, raw)
		}
	}

	revision, err := cat.Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	equal, err := revision.Schema.Equal(schema)
	if err != nil || !equal {
		t.Fatalf("latest snapshot differs from rebuilt catalog: equal=%v err=%v", equal, err)
	}
}

func TestCanonicalSchemaJSONShape(t *testing.T) {
	schema := catalog.SchemaFromDefinitions([]catalog.TableDef{usersDef()})
	raw, err := schema.CanonicalJSON()
	if err != nil {
		t.Fatal(err)
	}
	want := `{"tables":[{"name":"users","columns":[{"name":"id","type":"int64"},{"name":"name","type":"text"},{"name":"age","type":"int64","nullable":true}],"primary_key":["id"],"indexes":[{"name":"users_name_idx","columns":["name"]}]}]}`
	if string(raw) != want {
		t.Fatalf("canonical JSON:\n got %s\nwant %s", raw, want)
	}
}

func TestValidateCurrentSchemaDetectsPhysicalDrift(t *testing.T) {
	cat, store, ctx := newCatalog(t)
	table, err := cat.CreateTable(ctx, usersDef())
	if err != nil {
		t.Fatal(err)
	}
	if err := cat.ValidateCurrentSchema(ctx); err != nil {
		t.Fatal(err)
	}

	key := []byte("/rad/catalog/table/" + table.ID)
	raw, ok, err := store.Get(ctx, key)
	if err != nil || !ok {
		t.Fatalf("read physical table: ok=%v err=%v", ok, err)
	}
	var physical catalog.Table
	if err := json.Unmarshal(raw, &physical); err != nil {
		t.Fatal(err)
	}
	physical.Columns = append(physical.Columns, catalog.Column{
		ID: "c-untracked", Name: "untracked", Type: catalog.TypeText, Nullable: true,
	})
	raw, err = json.Marshal(physical)
	if err != nil {
		t.Fatal(err)
	}
	if err := store.Put(ctx, key, raw); err != nil {
		t.Fatal(err)
	}

	err = cat.ValidateCurrentSchema(ctx)
	if err == nil {
		t.Fatal("physical catalog drift was not detected")
	}
	reason, ok := reject.ReasonOf(err)
	if !ok || reason != reject.ReasonCatalogDrift {
		t.Fatalf("drift reason = %q, %v; want %q", reason, err, reject.ReasonCatalogDrift)
	}
}

func TestBuildSchemaRejectsDanglingPhysicalReference(t *testing.T) {
	_, err := catalog.BuildSchema([]catalog.Table{{
		ID: "t1", Name: "orders",
		Columns:    []catalog.Column{{ID: "c1", Name: "id", Type: catalog.TypeInt64}},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKey{{
			ID: "fk2", Name: "orders_user_fk", Columns: []string{"id"},
			RefTableID: "missing", RefColumns: []string{"id"},
		}},
	}})
	if err == nil {
		t.Fatal("dangling physical foreign key was accepted")
	}
	reason, ok := reject.ReasonOf(err)
	if !ok || reason != reject.ReasonCatalogDrift {
		t.Fatalf("reason = %q, %v; want %q", reason, err, reject.ReasonCatalogDrift)
	}
}

func containsJSONValue(document []byte, want string) bool {
	var value any
	if err := json.Unmarshal(document, &value); err != nil {
		return false
	}
	return containsValue(value, want)
}

func containsValue(value any, want string) bool {
	switch value := value.(type) {
	case string:
		return value == want
	case []any:
		for _, item := range value {
			if containsValue(item, want) {
				return true
			}
		}
	case map[string]any:
		for _, item := range value {
			if containsValue(item, want) {
				return true
			}
		}
	}
	return false
}
