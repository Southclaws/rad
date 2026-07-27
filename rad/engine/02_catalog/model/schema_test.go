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

func TestBuildSchemaExposesInsertDefaultNotHistoricalMissingValue(t *testing.T) {
	schema, err := model.BuildSchema([]model.Table{{
		ID: "t1", SchemaID: 1, Name: "items",
		Columns: []model.Column{{
			ID: "c1", SchemaID: 1, Name: "status", Type: model.TypeText,
			InsertDefault: &model.Default{Text: "pending"},
			MissingValue:  &model.Default{Text: "active"},
		}},
		PrimaryKey: []string{"status"},
	}})
	if err != nil {
		t.Fatal(err)
	}
	got := schema.Tables[0].Columns[0].Default
	if got == nil || got.Text != "pending" {
		t.Fatalf("canonical default = %+v, want pending", got)
	}
}

func TestBuildSchemaRejectsGeneratorHistoricalMissingValue(t *testing.T) {
	_, err := model.BuildSchema([]model.Table{{
		ID: "t1", SchemaID: 1, Name: "items",
		Columns: []model.Column{{
			ID: "c1", SchemaID: 1, Name: "id", Type: model.TypeText,
			MissingValue: &model.Default{Func: model.DefaultUUID},
		}},
		PrimaryKey: []string{"id"},
	}})
	if err == nil {
		t.Fatal("generator historical missing value was accepted")
	}
}
