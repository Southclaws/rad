package change_test

import (
	"context"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func TestServiceGroupsOneDirectMutationIntoOneRevision(t *testing.T) {
	ctx := context.Background()
	database, err := kvslate.Open("catalog-change-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = database.Close() })

	service := change.New(database)
	created, err := service.CreateTable(ctx, model.TableDef{
		Name:       "users",
		Columns:    []model.ColumnDef{{Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if created.SchemaID == 0 || created.Columns[0].SchemaID == 0 {
		t.Fatalf("logical IDs were not assigned: %+v", created)
	}
	revision, err := service.Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if revision.Version != 1 || revision.Schema.Tables[0].Name != "users" {
		t.Fatalf("unexpected revision: %+v", revision)
	}
}
