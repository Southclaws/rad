package catalog_test

import (
	"context"
	"encoding/json"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	catalogstore "github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestRevisionHistoryStartsAtZero(t *testing.T) {
	cat, _, ctx := newCatalog(t)

	revision, err := cat.Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if revision.Version != 0 || !revision.CreatedAt.IsZero() {
		t.Fatalf("fresh revision = %+v, want version zero without a timestamp", revision)
	}
	empty, err := revision.Schema.CanonicalJSON()
	if err != nil || string(empty) != `{}` {
		t.Fatalf("version zero schema = %s, %v; want {}", empty, err)
	}
	history, err := cat.Revisions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(history) != 0 {
		t.Fatalf("fresh history = %+v, want empty", history)
	}
	if err := cat.ValidateCurrentSchema(ctx); err != nil {
		t.Fatalf("validate version zero: %v", err)
	}
}

func TestCatalogPublicationsRetainImmutableTableDefinitions(t *testing.T) {
	cat, database, ctx := newCatalog(t)
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatal(err)
	}
	before, ok, err := cat.GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("before: ok=%v err=%v", ok, err)
	}
	if before.DefinitionGeneration == 0 {
		t.Fatal("created table has no definition generation")
	}
	assertDefinitionName(t, ctx, database, before.SchemaID, before.DefinitionGeneration, "users")

	if err := cat.RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	after, ok, err := cat.GetTable(ctx, "people")
	if err != nil || !ok {
		t.Fatalf("after: ok=%v err=%v", ok, err)
	}
	if after.SchemaID != before.SchemaID || after.ID != before.ID {
		t.Fatalf("rename changed stable identity: before=%+v after=%+v", before, after)
	}
	if after.DefinitionGeneration <= before.DefinitionGeneration {
		t.Fatalf("definition generation did not advance: %d -> %d", before.DefinitionGeneration, after.DefinitionGeneration)
	}
	assertDefinitionName(t, ctx, database, before.SchemaID, before.DefinitionGeneration, "users")
	assertDefinitionName(t, ctx, database, after.SchemaID, after.DefinitionGeneration, "people")
	version, generation, ok, err := catalogstore.DefinitionHead(ctx, database, after.SchemaID)
	if err != nil || !ok || version != 2 || generation != after.DefinitionGeneration {
		t.Fatalf("definition head = version %d generation %d ok=%v err=%v", version, generation, ok, err)
	}
}

func assertDefinitionName(t *testing.T, ctx context.Context, database interface {
	Get(context.Context, []byte) ([]byte, bool, error)
}, id model.SchemaID, generation uint64, want string) {
	t.Helper()
	raw, ok, err := database.Get(ctx, catalogstore.TableDefinitionKey(id, generation))
	if err != nil || !ok {
		t.Fatalf("definition %d/%d: ok=%v err=%v", id, generation, ok, err)
	}
	var table model.Table
	if err := json.Unmarshal(raw, &table); err != nil {
		t.Fatal(err)
	}
	if table.Name != want {
		t.Fatalf("definition %d/%d name = %q, want %q", id, generation, table.Name, want)
	}
}

func TestCorruptRevisionUsesCatalogRejectReason(t *testing.T) {
	cat, store, ctx := newCatalog(t)
	if err := store.Put(ctx, []byte("/rad/catalog/meta/schema_version"), []byte("not-a-version")); err != nil {
		t.Fatal(err)
	}
	_, err := cat.Revision(ctx)
	if err == nil {
		t.Fatal("corrupt schema version was accepted")
	}
	reason, ok := reject.ReasonOf(err)
	if !ok || reason != reject.ReasonCatalogCorrupt {
		t.Fatalf("reason = %q, %v; want %q", reason, err, reject.ReasonCatalogCorrupt)
	}
}

func TestDirectChangesRecordIndividualRevisions(t *testing.T) {
	cat, store, ctx := newCatalog(t)
	if _, err := cat.InitMode(ctx, model.ModeDirect); err != nil {
		t.Fatal(err)
	}
	assertVersion := func(want uint64) {
		t.Helper()
		revision, err := cat.Revision(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if revision.Version != want {
			t.Fatalf("schema version = %d, want %d", revision.Version, want)
		}
		if want > 0 && revision.CreatedAt.IsZero() {
			t.Fatalf("schema version %d has no timestamp", want)
		}
	}

	assertVersion(0)
	if _, err := cat.CreateTable(ctx, usersDef()); err != nil {
		t.Fatal(err)
	}
	assertVersion(1)
	if err := cat.ValidateCurrentSchema(ctx); err != nil {
		t.Fatalf("validate v1: %v", err)
	}
	if _, err := cat.CreateColumn(ctx, "users", model.ColumnDef{Name: "bio", Type: model.TypeText, Nullable: true}); err != nil {
		t.Fatal(err)
	}
	assertVersion(2)
	if err := cat.ValidateCurrentSchema(ctx); err != nil {
		t.Fatalf("validate v2: %v", err)
	}
	if err := cat.RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	assertVersion(3)
	if err := cat.ValidateCurrentSchema(ctx); err != nil {
		t.Fatalf("validate v3: %v", err)
	}

	// Successful no-ops and rejected changes do not describe a new schema.
	if err := cat.RenameTable(ctx, "people", "people"); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateColumn(ctx, "people", model.ColumnDef{Name: "bio", Type: model.TypeText, Nullable: true}); err == nil {
		t.Fatal("duplicate column should fail")
	}
	assertVersion(3)

	history, err := cat.Revisions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(history) != 3 {
		t.Fatalf("history = %+v, want three entries", history)
	}
	for i, revision := range history {
		if revision.Version != uint64(i+1) || revision.CreatedAt.IsZero() {
			t.Fatalf("history[%d] = %+v", i, revision)
		}
	}
	if history[0].Schema.Tables[0].Name != "users" || len(history[0].Schema.Tables[0].Columns) != 3 {
		t.Fatalf("v1 schema = %+v", history[0].Schema)
	}
	if history[1].Schema.Tables[0].Name != "users" || len(history[1].Schema.Tables[0].Columns) != 4 {
		t.Fatalf("v2 schema = %+v", history[1].Schema)
	}
	if history[2].Schema.Tables[0].Name != "people" || len(history[2].Schema.Tables[0].Columns) != 4 {
		t.Fatalf("v3 schema = %+v", history[2].Schema)
	}
	for i, revision := range history {
		raw, err := json.Marshal(revision)
		if err != nil {
			t.Fatal(err)
		}
		var decoded model.Revision
		if err := json.Unmarshal(raw, &decoded); err != nil {
			t.Fatal(err)
		}
		equal, err := revision.Schema.Equal(decoded.Schema)
		if err != nil || !equal {
			t.Fatalf("revision %d schema did not round-trip: equal=%v err=%v", i+1, equal, err)
		}
	}

	// A new catalog handle over the same store sees the persisted counter and
	// history; neither is process-local state.
	reopened := catalog.New(store)
	revision, err := reopened.Revision(ctx)
	if err != nil || revision.Version != 3 {
		t.Fatalf("reopened revision = %+v, %v", revision, err)
	}
	reopenedHistory, err := reopened.Revisions(ctx)
	if err != nil || len(reopenedHistory) != 3 {
		t.Fatalf("reopened history = %+v, %v", reopenedHistory, err)
	}
}
