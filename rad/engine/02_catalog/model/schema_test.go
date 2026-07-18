package model_test

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func TestSchemaCanonicalizationIgnoresDeclarationOrder(t *testing.T) {
	left := model.SchemaFromDefinitions([]model.TableDef{
		{ID: 2, Name: "orders", Columns: []model.ColumnDef{{ID: 1, Name: "id", Type: model.TypeInt64}}, PrimaryKey: []string{"id"}},
		{ID: 1, Name: "users", Columns: []model.ColumnDef{{ID: 1, Name: "id", Type: model.TypeText}}, PrimaryKey: []string{"id"}},
	})
	right := model.SchemaFromDefinitions([]model.TableDef{left.Tables[0], left.Tables[1]})

	equal, err := left.Equal(right)
	if err != nil {
		t.Fatal(err)
	}
	if !equal || left.Tables[0].Name != "users" {
		t.Fatalf("schemas were not canonicalized: %+v", left.Tables)
	}
}

func TestBuildSchemaRejectsDuplicateLogicalIdentity(t *testing.T) {
	_, err := model.BuildSchema([]model.Table{
		{ID: "t1", SchemaID: 1, Name: "users"},
		{ID: "t2", SchemaID: 1, Name: "accounts"},
	})
	if err == nil {
		t.Fatal("expected duplicate schema ID to be rejected")
	}
}
