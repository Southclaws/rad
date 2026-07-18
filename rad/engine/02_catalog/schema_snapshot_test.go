package catalog_test

import (
	"encoding/json"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestCanonicalSchemaRebuildsCatalogWithoutPhysicalIDs(t *testing.T) {
	cat, _, ctx := newCatalog(t)
	users, err := cat.CreateTable(ctx, model.TableDef{
		Name: "users",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeText, Default: &model.Default{Func: model.DefaultUUID}},
			{Name: "email", Type: model.TypeText, Format: "email"},
		},
		PrimaryKey: []string{"id"},
		Indexes: []model.IndexDef{
			{Name: "users_email_unique", Columns: []string{"email"}, Unique: true},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	orders, err := cat.CreateTable(ctx, model.TableDef{
		Name: "orders",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "user_id", Type: model.TypeText},
			{Name: "paid", Type: model.TypeBool, Default: &model.Default{Bool: false}},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []model.ForeignKeyDef{
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
	if len(schema.Tables) != 2 || schema.Tables[0].Name != "users" || schema.Tables[1].Name != "orders" {
		t.Fatalf("canonical table order = %+v", schema.Tables)
	}
	if schema.Tables[1].ForeignKeys[0].RefTable != "users" {
		t.Fatalf("foreign key did not resolve to a table name: %+v", schema.Tables[1].ForeignKeys)
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
	definition := usersDef()
	definition.ID = 7
	for i := range definition.Columns {
		definition.Columns[i].ID = model.SchemaID(i + 1)
	}
	schema := model.SchemaFromDefinitions([]model.TableDef{definition})
	raw, err := schema.CanonicalJSON()
	if err != nil {
		t.Fatal(err)
	}
	want := `{"tables":[{"id":7,"name":"users","columns":[{"id":1,"name":"id","type":"int64"},{"id":2,"name":"name","type":"text"},{"id":3,"name":"age","type":"int64","nullable":true}],"primary_key":["id"],"indexes":[{"name":"users_name_idx","columns":["name"]}]}]}`
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
	var physical model.Table
	if err := json.Unmarshal(raw, &physical); err != nil {
		t.Fatal(err)
	}
	physical.Columns = append(physical.Columns, model.Column{
		ID: "c-untracked", SchemaID: 99, Name: "untracked", Type: model.TypeText, Nullable: true,
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
	_, err := model.BuildSchema([]model.Table{{
		ID: "t1", SchemaID: 1, Name: "orders",
		Columns:    []model.Column{{ID: "c1", SchemaID: 1, Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
		ForeignKeys: []model.ForeignKey{{
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
