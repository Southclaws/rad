package store_test

import (
	"context"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

func TestReaderRoundTripsTableMetadata(t *testing.T) {
	ctx := context.Background()
	database, err := kvslate.Open("catalog-store-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = database.Close() })

	table := model.Table{
		ID: "t1", SchemaID: 1, Name: "users",
		Columns:    []model.Column{{ID: "c2", SchemaID: 1, Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	}
	if err := store.SaveTable(ctx, database, table); err != nil {
		t.Fatal(err)
	}
	if err := database.Put(ctx, store.TableNameKey(table.Name), []byte(table.ID)); err != nil {
		t.Fatal(err)
	}

	got, ok, err := store.New(database).GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("GetTable: ok=%v err=%v", ok, err)
	}
	if got.ID != table.ID || got.Columns[0].Type != model.TypeInt64 {
		t.Fatalf("round trip changed table: %+v", got)
	}
}

func TestRevisionPersistsCanonicalSchema(t *testing.T) {
	ctx := context.Background()
	database, err := kvslate.Open("catalog-revision-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = database.Close() })

	table := model.Table{ID: "t1", SchemaID: 1, Name: "users"}
	if err := store.SaveTable(ctx, database, table); err != nil {
		t.Fatal(err)
	}
	revision, err := store.BumpRevision(ctx, database)
	if err != nil {
		t.Fatal(err)
	}
	if revision.Version != 1 || len(revision.Schema.Tables) != 1 {
		t.Fatalf("unexpected revision: %+v", revision)
	}
}
