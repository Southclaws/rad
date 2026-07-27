package exec

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
)

func TestTableReclamationIsBoundedResumableAndSnapshotSafe(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	_, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []model.IndexDef{{Name: "scratch_value_idx", Columns: []string{"value"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	table, _, _ := eng.Catalog().GetTable(ctx, "scratch")
	for i := range 5 {
		if err := eng.Insert(ctx, "scratch", lir.Row{"id": lir.Int64(int64(i)), "value": lir.Text("v")}); err != nil {
			t.Fatal(err)
		}
	}

	oldSnapshot, err := eng.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	defer oldSnapshot.Rollback()
	if err := eng.Catalog().DeleteTable(ctx, "scratch"); err != nil {
		t.Fatal(err)
	}
	id := store.TableReclamationID(table.ID)
	ownerA, claimed, err := eng.claimReclamation(ctx, id)
	if err != nil || !claimed {
		t.Fatalf("claim A: claimed=%v err=%v", claimed, err)
	}
	progress, err := eng.stepReclamation(ctx, id, ownerA, 1)
	if err != nil {
		t.Fatal(err)
	}
	if progress.State != model.ReclamationReclaiming || progress.BatchID != 1 || progress.ItemsReclaimed != 1 {
		t.Fatalf("first bounded batch = %+v", progress)
	}

	reopened := New(eng.store, catalog.New(eng.store), withAutomaticReclamation(false))
	t.Cleanup(func() { _ = reopened.Close() })
	ownerB, claimed, err := reopened.claimReclamation(ctx, id)
	if err != nil || !claimed || ownerB <= ownerA {
		t.Fatalf("takeover: owner=%d claimed=%v err=%v", ownerB, claimed, err)
	}
	if _, err := eng.stepReclamation(ctx, id, ownerA, 1); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale worker checkpoint = %v, want conflict", err)
	}
	if err := reopened.runReclamation(ctx, id, ownerB, 1); err != nil {
		t.Fatal(err)
	}
	assertReclamationState(t, ctx, eng, id, model.ReclamationReclaimed)
	assertRangeCount(t, ctx, eng.store, codec.DataPrefix(table.ID), nil, 0)
	for _, index := range table.Indexes {
		assertRangeCount(t, ctx, eng.store, codec.IndexPrefix(table.ID, index.ID), nil, 0)
	}

	// Slate owns row-version retention: a snapshot opened before logical deletion
	// still observes pre-reclamation keys after the new state has tombstoned
	// them, while new binding can no longer resolve the table.
	assertRangeCount(t, ctx, oldSnapshot, codec.DataPrefix(table.ID), nil, 5)
	if _, _, err := eng.GetByPrimaryKey(ctx, "scratch", lir.Row{"id": lir.Int64(0)}); err == nil {
		t.Fatal("new binding resolved a reclaimed table")
	}
	if raw, ok, err := eng.store.Get(ctx, store.TableExistenceFenceKey(table.ID)); err != nil || !ok || len(raw) == 0 {
		t.Fatalf("retirement fence was reclaimed: raw=%q ok=%v err=%v", raw, ok, err)
	}
}

func TestColumnReclamationRewritesOnlyRetiredCells(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	table, _, _ := eng.Catalog().GetTable(ctx, "users")
	age, _ := table.Column("age")
	for i := range 4 {
		row := lir.Row{"id": lir.Int64(int64(i + 1)), "name": lir.Text("person")}
		if i%2 == 0 {
			row["age"] = lir.Int64(int64(20 + i))
		}
		if err := eng.Insert(ctx, "users", row); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := eng.Catalog().DeleteColumn(ctx, "users", "age"); err != nil {
		t.Fatal(err)
	}
	id := store.ColumnReclamationID(table.ID, age.ID)
	drainReclamation(t, ctx, eng, id, 1)

	start := codec.DataPrefix(table.ID)
	it, err := eng.store.Scan(ctx, start, keyenc.PrefixEnd(start))
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	rows := 0
	for it.Next() {
		_, changed, err := codec.RemoveColumn(it.Value(), age.ID)
		if err != nil {
			t.Fatal(err)
		}
		if changed {
			t.Fatalf("row %q still contains retired column %s", it.Key(), age.ID)
		}
		rows++
	}
	if err := it.Err(); err != nil || rows != 4 {
		t.Fatalf("rows=%d err=%v", rows, err)
	}
	if raw, ok, err := eng.store.Get(ctx, store.ColumnValueFenceKey(table.ID, age.ID)); err != nil || !ok || len(raw) == 0 {
		t.Fatalf("column retirement fence was reclaimed: raw=%q ok=%v err=%v", raw, ok, err)
	}
	row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok || len(row) != 2 {
		t.Fatalf("post-reclaim row=%v ok=%v err=%v", row, ok, err)
	}
}

func TestIndexReclamationUsesAccessFenceAndPreservesOldSnapshot(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "indexed", 7)); err != nil {
		t.Fatal(err)
	}
	table, _, _ := eng.Catalog().GetTable(ctx, "users")
	index, _ := table.Index("users_name_idx")
	oldSnapshot, err := eng.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	defer oldSnapshot.Rollback()

	if err := eng.Catalog().DeleteIndex(ctx, "users", index.Name); err != nil {
		t.Fatal(err)
	}
	drainReclamation(t, ctx, eng, store.IndexReclamationID(table.ID, index.ID), 1)
	assertRangeCount(t, ctx, eng.store, codec.IndexPrefix(table.ID, index.ID), nil, 0)
	assertRangeCount(t, ctx, oldSnapshot, codec.IndexPrefix(table.ID, index.ID), nil, 1)

	postDeletion, err := eng.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	defer postDeletion.Rollback()
	if _, err := rowstore.ScanIndexRange(ctx, postDeletion, table, index, nil, nil); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("old index plan admitted after retirement: %v", err)
	}
	if raw, ok, err := eng.store.Get(ctx, store.IndexAccessFenceKey(table.ID, index.ID)); err != nil || !ok || len(raw) == 0 {
		t.Fatalf("index retirement fence was reclaimed: raw=%q ok=%v err=%v", raw, ok, err)
	}
}

func TestCancelledIndexReclamationRemovesPartialWorkAndDeltas(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	for i := range 3 {
		if err := eng.Insert(ctx, "users", userRow(i+1, "before", int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_age_online", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "users", userRow(9, "delta", 9)); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err != nil {
		t.Fatal(err)
	}
	drainReclamation(t, ctx, eng, store.CancelledIndexReclamationID(transition.ID), 1)
	assertRangeCount(t, ctx, eng.store, codec.IndexPrefix(transition.TableID, transition.Index.ID), nil, 0)
	start, end := store.DeltaRange(transition.ID)
	assertRangeCount(t, ctx, eng.store, start, end, 0)
	if _, ok, err := store.GetTransition(ctx, eng.store, transition.ID); err != nil || !ok {
		t.Fatalf("terminal transition diagnostics were removed: ok=%v err=%v", ok, err)
	}
	compacted, _, err := store.GetTransition(ctx, eng.store, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if compacted.CompactedAt.IsZero() || compacted.OwnerEpoch != 0 || compacted.BasePosition != "" {
		t.Fatalf("cancelled transition was not compacted: %+v", compacted)
	}
}

func TestReadyIndexReclamationRemovesDeltasNotPublishedIndex(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "ready", 8)); err != nil {
		t.Fatal(err)
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_age_online", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	ready, err := eng.runIndexBuild(ctx, transition.ID, 1)
	if err != nil {
		t.Fatal(err)
	}
	drainReclamation(t, ctx, eng, store.TransitionDeltaReclamationID(transition.ID), 1)
	start, end := store.DeltaRange(transition.ID)
	assertRangeCount(t, ctx, eng.store, start, end, 0)
	assertRangeCount(t, ctx, eng.store, codec.IndexPrefix(ready.TableID, ready.Index.ID), nil, 1)
	rows, err := eng.ScanIndex(ctx, "users", "users_age_online", lir.Row{"age": lir.Int64(8)})
	if err != nil || len(rows) != 1 {
		t.Fatalf("published index after delta reclamation: rows=%v err=%v", rows, err)
	}
	compacted, _, err := store.GetTransition(ctx, eng.store, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if compacted.CompactedAt.IsZero() || compacted.OwnerEpoch != 0 || compacted.BarrierPosition != "" {
		t.Fatalf("ready transition was not compacted: %+v", compacted)
	}
}

func TestFailedIndexCleanupIsBoundedAndLeavesControlDiagnostics(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "duplicate", 1)); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "users", userRow(2, "duplicate", 2)); err != nil {
		t.Fatal(err)
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_failed_cleanup", Columns: []string{"name"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	failed := driveIndexBuildToTerminal(t, ctx, eng, transition.ID, owner, 1)
	if failed.State != model.TransitionFailed || failed.LastError == "" {
		t.Fatalf("failed transition = %+v", failed)
	}
	table, _, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	if _, exists := table.Index("users_name_failed_cleanup"); exists {
		t.Fatal("failed logical index definition remained in the table")
	}
	reclamationID := store.FailedIndexReclamationID(transition.ID)
	drainReclamation(t, ctx, eng, reclamationID, 1)
	assertRangeCount(t, ctx, eng.store, codec.IndexPrefix(transition.TableID, transition.Index.ID), nil, 0)
	start, end := store.DeltaRange(transition.ID)
	assertRangeCount(t, ctx, eng.store, start, end, 0)
	diagnostic, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if diagnostic.State != model.TransitionFailed || diagnostic.LastError == "" ||
		diagnostic.CompactedAt.IsZero() || diagnostic.OwnerEpoch != 0 {
		t.Fatalf("compacted failure diagnostics = %+v", diagnostic)
	}
	retry, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_failed_cleanup", Columns: []string{"name"},
	})
	if err != nil {
		t.Fatalf("failed cleanup did not release logical name: %v", err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, retry.ID); err != nil {
		t.Fatal(err)
	}
}

func TestAutomaticReclamationRunsWithoutMaintenanceCall(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "automatic", 1)); err != nil {
		t.Fatal(err)
	}
	table, _, _ := eng.Catalog().GetTable(ctx, "users")
	index, _ := table.Index("users_name_idx")
	if err := eng.Catalog().DeleteIndex(ctx, "users", index.Name); err != nil {
		t.Fatal(err)
	}
	id := store.IndexReclamationID(table.ID, index.ID)
	deadline := time.Now().Add(5 * time.Second)
	for {
		reclamation, ok, err := store.GetReclamation(ctx, eng.store, id)
		if err != nil {
			t.Fatal(err)
		}
		if ok && reclamation.State == model.ReclamationReclaimed {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("automatic reclamation did not finish: %+v", reclamation)
		}
		time.Sleep(time.Millisecond)
	}
	assertRangeCount(t, ctx, eng.store, codec.IndexPrefix(table.ID, index.ID), nil, 0)
}

func TestReclamationRevalidatesTypedRetirementBeforeEveryBatch(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	table, _, _ := eng.Catalog().GetTable(ctx, "users")
	if err := store.QueueReclamation(ctx, eng.store, model.Reclamation{
		ID: "adversarial-live-table", Kind: model.ReclamationTable,
		RetiredCatalogVersion: 999, TableID: table.ID, TableSchemaID: table.SchemaID,
	}); err != nil {
		t.Fatal(err)
	}
	owner, claimed, err := eng.claimReclamation(ctx, "adversarial-live-table")
	if err != nil || !claimed {
		t.Fatalf("claim: claimed=%v err=%v", claimed, err)
	}
	if _, err := eng.stepReclamation(ctx, "adversarial-live-table", owner, 128); err == nil || !strings.Contains(err.Error(), "live table") {
		t.Fatalf("forged reclamation was accepted: %v", err)
	}
	if err := eng.Insert(ctx, "users", userRow(1, "still-live", 1)); err != nil {
		t.Fatalf("forged reclamation damaged live table: %v", err)
	}
}

func TestEngineStartupResumesDurableReclamation(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "restart", 1)); err != nil {
		t.Fatal(err)
	}
	table, _, _ := eng.Catalog().GetTable(ctx, "users")
	index, _ := table.Index("users_name_idx")
	if err := eng.Catalog().DeleteIndex(ctx, "users", index.Name); err != nil {
		t.Fatal(err)
	}
	id := store.IndexReclamationID(table.ID, index.ID)
	if reclamation, ok, err := store.GetReclamation(ctx, eng.store, id); err != nil || !ok || reclamation.State != model.ReclamationPending {
		t.Fatalf("durable pending state = %+v ok=%v err=%v", reclamation, ok, err)
	}

	reopened := New(eng.store, catalog.New(eng.store))
	t.Cleanup(func() { _ = reopened.Close() })
	deadline := time.Now().Add(5 * time.Second)
	for {
		reclamation, ok, err := store.GetReclamation(ctx, reopened.store, id)
		if err != nil {
			t.Fatal(err)
		}
		if ok && reclamation.State == model.ReclamationReclaimed {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("startup did not resume reclamation: %+v", reclamation)
		}
		time.Sleep(time.Millisecond)
	}
	assertRangeCount(t, ctx, reopened.store, codec.IndexPrefix(table.ID, index.ID), nil, 0)
}

func TestTableDeletionRequiresTerminalPhysicalTransitions(t *testing.T) {
	eng, ctx := setup(t)
	_, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	transition, err := eng.startIndexBuild(ctx, "scratch", model.IndexDef{Name: "scratch_value_online", Columns: []string{"value"}})
	if err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().DeleteTable(ctx, "scratch"); err == nil || !strings.Contains(err.Error(), "active transition") {
		t.Fatalf("table deletion with active transition = %v", err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().DeleteTable(ctx, "scratch"); err != nil {
		t.Fatalf("table deletion after transition cancellation: %v", err)
	}
}

func TestDefinitionReclamationIsIndependentFromSchemaHistory(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	before, _, _ := eng.Catalog().GetTable(ctx, "users")
	if err := eng.Catalog().RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	after, _, _ := eng.Catalog().GetTable(ctx, "people")
	id := store.TableDefinitionReclamationID(before.SchemaID, before.DefinitionGeneration)
	drainReclamation(t, ctx, eng, id, 1)
	if _, ok, err := eng.store.Get(ctx, store.TableDefinitionKey(before.SchemaID, before.DefinitionGeneration)); err != nil || ok {
		t.Fatalf("old physical definition remains: ok=%v err=%v", ok, err)
	}
	if _, ok, err := eng.store.Get(ctx, store.TableDefinitionKey(after.SchemaID, after.DefinitionGeneration)); err != nil || !ok {
		t.Fatalf("current physical definition missing: ok=%v err=%v", ok, err)
	}
	revisions, err := eng.Catalog().Revisions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(revisions) < 2 || revisions[len(revisions)-2].Schema.Tables[0].Name != "users" ||
		revisions[len(revisions)-1].Schema.Tables[0].Name != "people" {
		t.Fatalf("logical schema history changed during physical reclamation: %+v", revisions)
	}
}

func TestCatalogHistoryCompactionIsIndependentFromBindingAndReclamationSafety(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	before, _, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	pin := model.RetentionPin{
		ID:        "prepared-users-before-rename",
		OwnerKind: model.RetentionOwnerPreparedPlan,
		OwnerID:   "prepared-plan-1",
		Resource: model.RetentionResource{
			Kind:                 model.RetentionTableDefinition,
			TableSchemaID:        before.SchemaID,
			DefinitionGeneration: before.DefinitionGeneration,
		},
	}
	if err := eng.retain(ctx, pin); err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}

	txn, err := eng.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	deleted, more, err := store.CompactRevisionHistoryBatch(ctx, txn, 1, 128)
	if err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if more {
		_ = txn.Rollback()
		t.Fatal("one compaction batch did not reach the history horizon")
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	if deleted < 2 {
		t.Fatalf("compacted revisions = %d, want at least the two superseded setup revisions", deleted)
	}

	revisions, err := eng.Catalog().Revisions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(revisions) != 1 {
		t.Fatalf("retained canonical revisions = %+v", revisions)
	}
	if _, ok, err := eng.Catalog().GetTable(ctx, "people"); err != nil || !ok {
		t.Fatalf("current binding after history compaction: ok=%v err=%v", ok, err)
	}
	if _, ok, err := eng.Catalog().GetTable(ctx, "users"); err != nil || ok {
		t.Fatalf("superseded name remained bindable: ok=%v err=%v", ok, err)
	}
	oldDefinitionKey := store.TableDefinitionKey(before.SchemaID, before.DefinitionGeneration)
	if _, ok, err := eng.store.Get(ctx, oldDefinitionKey); err != nil || !ok {
		t.Fatalf("history compaction removed pinned immutable definition: ok=%v err=%v", ok, err)
	}

	reclamationID := store.TableDefinitionReclamationID(before.SchemaID, before.DefinitionGeneration)
	owner, claimed, err := eng.claimReclamation(ctx, reclamationID)
	if err != nil || !claimed {
		t.Fatalf("claim definition reclamation: owner=%d claimed=%v err=%v", owner, claimed, err)
	}
	if _, err := eng.stepReclamation(ctx, reclamationID, owner, 1); !errors.Is(err, ErrRetentionPinned) {
		t.Fatalf("definition reclamation after history compaction = %v, want retained", err)
	}
	if err := eng.releaseRetention(ctx, pin.ID); err != nil {
		t.Fatal(err)
	}
	if err := eng.runReclamation(ctx, reclamationID, owner, 1); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := eng.store.Get(ctx, oldDefinitionKey); err != nil || ok {
		t.Fatalf("released immutable definition remains: ok=%v err=%v", ok, err)
	}
	if _, ok, err := eng.Catalog().GetTable(ctx, "people"); err != nil || !ok {
		t.Fatalf("binding depended on reclaimed history/definition: ok=%v err=%v", ok, err)
	}
}

func drainReclamation(t *testing.T, ctx context.Context, eng *Engine, id string, batchSize int) {
	t.Helper()
	owner, claimed, err := eng.claimReclamation(ctx, id)
	if err != nil || !claimed {
		t.Fatalf("claim %s: claimed=%v err=%v", id, claimed, err)
	}
	if err := eng.runReclamation(ctx, id, owner, batchSize); err != nil {
		t.Fatal(err)
	}
	assertReclamationState(t, ctx, eng, id, model.ReclamationReclaimed)
}

func assertReclamationState(t *testing.T, ctx context.Context, eng *Engine, id string, want model.ReclamationState) {
	t.Helper()
	reclamation, ok, err := store.GetReclamation(ctx, eng.store, id)
	if err != nil || !ok || reclamation.State != want {
		t.Fatalf("reclamation %s = %+v ok=%v err=%v, want %s", id, reclamation, ok, err, want)
	}
}

func assertRangeCount(t *testing.T, ctx context.Context, view kv.KV, start, end []byte, want int) {
	t.Helper()
	if end == nil {
		end = keyenc.PrefixEnd(start)
	}
	it, err := view.Scan(ctx, start, end)
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	count := 0
	for it.Next() {
		count++
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	if count != want {
		t.Fatalf("range [%q,%q) contains %d keys, want %d", start, end, count, want)
	}
}
