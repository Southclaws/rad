package store

import (
	"context"
	"fmt"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func openCatalogStore(t *testing.T) *kvslate.Store {
	t.Helper()
	database, err := kvslate.Open("catalog-store-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = database.Close() })
	return database
}

func TestReaderRoundTripsTableMetadata(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)

	table := model.Table{
		ID: "t1", SchemaID: 1, Name: "users",
		Columns:    []model.Column{{ID: "c2", SchemaID: 1, Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	}
	if err := SaveTable(ctx, database, table); err != nil {
		t.Fatal(err)
	}
	if err := database.Put(ctx, TableNameKey(table.Name), []byte(table.ID)); err != nil {
		t.Fatal(err)
	}

	got, ok, err := New(database).GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("GetTable: ok=%v err=%v", ok, err)
	}
	if got.ID != table.ID || got.Columns[0].Type != model.TypeInt64 {
		t.Fatalf("round trip changed table: %+v", got)
	}
}

func TestRevisionPersistsCanonicalSchema(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)

	table := model.Table{ID: "t1", SchemaID: 1, Name: "users"}
	if err := SaveTable(ctx, database, table); err != nil {
		t.Fatal(err)
	}
	revision, err := BumpRevision(ctx, database)
	if err != nil {
		t.Fatal(err)
	}
	if revision.Version != 1 || len(revision.Schema.Tables) != 1 {
		t.Fatalf("unexpected revision: %+v", revision)
	}
}

func TestRevisionCompactionAdvancesBoundedHorizonAndKeepsCurrent(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)

	table := model.Table{ID: "t1", SchemaID: 1, Name: "users", DefinitionGeneration: 1}
	for version := uint64(1); version <= 5; version++ {
		table.Name = fmt.Sprintf("users-v%d", version)
		table.DefinitionGeneration = version
		if err := SaveTable(ctx, database, table); err != nil {
			t.Fatal(err)
		}
		if revision, err := BumpRevision(ctx, database); err != nil || revision.Version != version {
			t.Fatalf("bump revision %d: revision=%+v err=%v", version, revision, err)
		}
	}

	compact := func(wantDeleted int, wantMore bool) {
		t.Helper()
		txn, err := database.Begin(ctx, kv.SerializableSnapshot)
		if err != nil {
			t.Fatal(err)
		}
		defer txn.Rollback()
		deleted, more, err := CompactRevisionHistoryBatch(ctx, txn, 2, 2)
		if err != nil {
			t.Fatal(err)
		}
		if deleted != wantDeleted || more != wantMore {
			t.Fatalf("compaction = deleted %d more=%v, want %d/%v", deleted, more, wantDeleted, wantMore)
		}
		if err := txn.Commit(ctx); err != nil {
			t.Fatal(err)
		}
	}
	compact(2, true)
	compact(1, false)
	compact(0, false)

	revisions, err := Revisions(ctx, database)
	if err != nil {
		t.Fatal(err)
	}
	if len(revisions) != 2 || revisions[0].Version != 4 || revisions[1].Version != 5 {
		t.Fatalf("retained revisions = %+v", revisions)
	}
	current, err := CurrentRevision(ctx, database)
	if err != nil || current.Version != 5 || current.Schema.Tables[0].Name != "users-v5" {
		t.Fatalf("current revision after compaction = %+v err=%v", current, err)
	}
	horizon, err := RevisionCompactedThrough(ctx, database)
	if err != nil || horizon != 3 {
		t.Fatalf("compacted-through horizon = %d err=%v", horizon, err)
	}
}
