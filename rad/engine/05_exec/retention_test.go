package exec

import (
	"context"
	"errors"
	"sync"
	"testing"
	"time"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestDurablePhysicalPinBlocksOnlyItsReclamation(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "retained", 7)); err != nil {
		t.Fatal(err)
	}
	table, _, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	index, _ := table.Index("users_name_idx")
	pin := model.RetentionPin{
		ID: "reader-users-name-index", OwnerKind: model.RetentionOwnerPhysicalReader, OwnerID: "reader-1",
		Resource: model.RetentionResource{
			Kind: model.RetentionPhysicalIndex, TableID: table.ID, IndexID: index.ID,
		},
	}
	if err := eng.retain(ctx, pin); err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().DeleteIndex(ctx, table.Name, index.Name); err != nil {
		t.Fatal(err)
	}
	id := store.IndexReclamationID(table.ID, index.ID)
	owner, claimed, err := eng.claimReclamation(ctx, id)
	if err != nil || !claimed {
		t.Fatalf("claim: owner=%d claimed=%v err=%v", owner, claimed, err)
	}
	if _, err := eng.stepReclamation(ctx, id, owner, 128); !errors.Is(err, ErrRetentionPinned) {
		t.Fatalf("pinned reclamation = %v, want retention block", err)
	}
	rows, err := eng.ScanIndex(ctx, "users", "users_name_idx", lir.Row{"name": lir.Text("retained")})
	if err == nil || len(rows) != 0 {
		t.Fatalf("logically deleted index remained bindable: rows=%v err=%v", rows, err)
	}
	horizons, err := eng.retentionHorizons(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(horizons.PhysicalArtifacts) != 1 || horizons.PhysicalArtifacts[0].PinCount != 1 ||
		horizons.PhysicalArtifacts[0].Resource != pin.Resource ||
		len(horizons.CatalogDefinitions) != 0 || len(horizons.DataSnapshots) != 0 ||
		len(horizons.TransitionDeltas) != 0 {
		t.Fatalf("retention horizons = %+v", horizons)
	}
	if err := eng.releaseRetention(ctx, pin.ID); err != nil {
		t.Fatal(err)
	}
	drainReclamation(t, ctx, eng, id, 1)
	assertRangeCount(t, ctx, eng.store, store.IndexAccessFenceKey(table.ID, index.ID), nil, 1)
}

func TestDurableDefinitionAndSnapshotPinsSurviveFreshEngine(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	table, _, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	pins := []model.RetentionPin{
		{
			ID: "plan-users-definition", OwnerKind: model.RetentionOwnerPreparedPlan, OwnerID: "plan-1",
			Resource: model.RetentionResource{
				Kind: model.RetentionTableDefinition, TableSchemaID: table.SchemaID,
				DefinitionGeneration: table.DefinitionGeneration,
			},
		},
		{
			ID: "replica-position", OwnerKind: model.RetentionOwnerReplica, OwnerID: "replica-1",
			Resource: model.RetentionResource{
				Kind: model.RetentionDataSnapshot, DataPosition: model.DataPosition("opaque-position"),
			},
		},
		{
			ID: "plan-users-definition-copy", OwnerKind: model.RetentionOwnerPreparedPlan, OwnerID: "plan-2",
			Resource: model.RetentionResource{
				Kind: model.RetentionTableDefinition, TableSchemaID: table.SchemaID,
				DefinitionGeneration: table.DefinitionGeneration,
			},
		},
	}
	for _, pin := range pins {
		if err := eng.retain(ctx, pin); err != nil {
			t.Fatal(err)
		}
	}
	reopened := New(
		eng.store,
		eng.Catalog(),
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	t.Cleanup(func() { _ = reopened.Close() })
	horizons, err := reopened.retentionHorizons(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(horizons.CatalogDefinitions) != 1 || len(horizons.DataSnapshots) != 1 ||
		horizons.CatalogDefinitions[0].PinCount != 2 || horizons.DataSnapshots[0].PinCount != 1 {
		t.Fatalf("reopened retention horizons = %+v", horizons)
	}
	for _, pin := range pins {
		if stored, ok, err := store.GetRetentionPin(ctx, reopened.store, pin.ID); err != nil || !ok ||
			stored.OwnerID != pin.OwnerID || stored.Resource != pin.Resource {
			t.Fatalf("reopened pin %q = %+v ok=%v err=%v", pin.ID, stored, ok, err)
		}
	}
}

func TestPinnedAutomaticReclamationBacksOffWithoutFailing(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "pinned-runner", 1)); err != nil {
		t.Fatal(err)
	}
	table, _, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	index, _ := table.Index("users_name_idx")
	pin := model.RetentionPin{
		ID:        "runner-index-pin",
		OwnerKind: model.RetentionOwnerPhysicalReader,
		OwnerID:   "reader-1",
		Resource: model.RetentionResource{
			Kind: model.RetentionPhysicalIndex, TableID: table.ID, IndexID: index.ID,
		},
	}
	if err := eng.retain(ctx, pin); err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().DeleteIndex(ctx, table.Name, index.Name); err != nil {
		t.Fatal(err)
	}

	runner := New(
		eng.store,
		catalog.New(eng.store),
		WithSchemaJobConfig(SchemaJobConfig{
			ReclamationBatchSize: 1,
			BatchesBeforeYield:   1,
			RetryBackoffMin:      time.Millisecond,
			RetryBackoffMax:      4 * time.Millisecond,
		}),
	)
	t.Cleanup(func() { _ = runner.Close() })
	reclamationID := store.IndexReclamationID(table.ID, index.ID)
	deadline := time.Now().Add(5 * time.Second)
	for {
		reclamation, ok, err := store.GetReclamation(ctx, runner.store, reclamationID)
		if err != nil {
			t.Fatal(err)
		}
		metrics, err := runner.schemaStorageMetrics(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if ok && reclamation.State == model.ReclamationReclaiming &&
			metrics.PinnedReclamations == 1 && metrics.RunnerBackoffs > 0 {
			break
		}
		if ok && reclamation.State == model.ReclamationFailed {
			t.Fatalf("retention wait became a failed reclamation: %+v", reclamation)
		}
		if time.Now().After(deadline) {
			t.Fatalf("runner did not report pinned backoff: reclamation=%+v metrics=%+v", reclamation, metrics)
		}
		time.Sleep(time.Millisecond)
	}
	if err := runner.releaseRetention(ctx, pin.ID); err != nil {
		t.Fatal(err)
	}
	waitForReclamationState(t, ctx, runner.store, reclamationID, model.ReclamationReclaimed)
}

func TestDiagnosticPinCanOutliveCleanupAndReleaseCompactsLater(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_diagnostics", Columns: []string{"age"},
	})
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
	if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err != nil {
		t.Fatal(err)
	}
	pin := model.RetentionPin{
		ID:        "transition-diagnostics-pin",
		OwnerKind: model.RetentionOwnerSchemaWorker,
		OwnerID:   "diagnostic-reader",
		Resource: model.RetentionResource{
			Kind: model.RetentionTransitionDiagnostics, TransitionID: transition.ID,
		},
	}
	if err := eng.retain(ctx, pin); err != nil {
		t.Fatal(err)
	}
	drainReclamation(t, ctx, eng, store.CancelledIndexReclamationID(transition.ID), 1)
	detailed, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if !detailed.CompactedAt.IsZero() || detailed.TableID == "" || detailed.OwnerEpoch == 0 {
		t.Fatalf("diagnostic pin did not preserve detailed transition: %+v", detailed)
	}

	runner := New(eng.store, catalog.New(eng.store), WithSchemaJobConfig(SchemaJobConfig{
		BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	t.Cleanup(func() { _ = runner.Close() })
	if err := runner.releaseRetention(ctx, pin.ID); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		compacted, err := runner.inspectSchemaTransition(ctx, transition.ID)
		if err != nil {
			t.Fatal(err)
		}
		if !compacted.CompactedAt.IsZero() {
			if compacted.TableID != "" || compacted.OwnerEpoch != 0 {
				t.Fatalf("released diagnostics were only partially compacted: %+v", compacted)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("released diagnostic record was not compacted: %+v", compacted)
		}
		time.Sleep(time.Millisecond)
	}
}

func TestSchemaJobIOBudgetBoundsIndividualBuildBatches(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	insertSchemaJobUsers(t, ctx, eng, 9, "io-budget")
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_io_budget", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}

	var mu sync.Mutex
	var checkpoints []uint64
	runner := New(
		eng.store,
		catalog.New(eng.store),
		WithSchemaJobConfig(SchemaJobConfig{
			IndexBatchSize:        128,
			BatchesBeforeYield:    8,
			IOBudgetItemsPerYield: 2,
			YieldInterval:         time.Millisecond,
		}),
		withSchemaJobHooks(schemaJobHooks{afterBatch: func(ctx context.Context, kind schemaJobKind, id string) {
			if kind != schemaJobIndexBuild || id != transition.ID {
				return
			}
			current, ok, err := store.GetTransition(ctx, eng.store, id)
			if err != nil || !ok {
				return
			}
			mu.Lock()
			checkpoints = append(checkpoints, current.RowsScanned)
			mu.Unlock()
		}}),
	)
	t.Cleanup(func() { _ = runner.Close() })
	ready := waitForTransitionState(t, ctx, runner, transition.ID, model.TransitionReady)
	if ready.RowsScanned != 9 {
		t.Fatalf("ready rows scanned = %d, want 9", ready.RowsScanned)
	}
	mu.Lock()
	observed := append([]uint64(nil), checkpoints...)
	mu.Unlock()
	var previous uint64
	for _, checkpoint := range observed {
		if checkpoint > previous && checkpoint-previous > 2 {
			t.Fatalf("I/O budget allowed scan jump %d -> %d; checkpoints=%v", previous, checkpoint, observed)
		}
		previous = checkpoint
	}
	metrics, err := runner.schemaStorageMetrics(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if metrics.RunnerBatches < 5 || metrics.RunnerItems < 9 {
		t.Fatalf("runner resource metrics = %+v", metrics)
	}
}
