package exec

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"testing"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestSchemaJobRunnerAutomaticallyBuildsIndexFromDurableState(t *testing.T) {
	eng, ctx := setup(t)
	insertSchemaJobUsers(t, ctx, eng, 33, "automatic")
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_automatic", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}

	automatic := New(eng.store, catalog.New(eng.store), WithSchemaJobConfig(SchemaJobConfig{
		IndexBatchSize: 7, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	t.Cleanup(func() { _ = automatic.Close() })
	ready := waitForTransitionState(t, ctx, automatic, transition.ID, model.TransitionReady)
	if ready.RowsScanned != 33 {
		t.Fatalf("automatic rows scanned = %d, want 33", ready.RowsScanned)
	}
	assertIndexEqualsTable(t, ctx, automatic, "users", "users_age_automatic")
}

func TestSchemaJobRunnerAutomaticallyResumesReplacementAndConstraintValidation(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	for i := range 23 {
		if err := eng.Insert(ctx, "users", userRow(i, fmt.Sprintf("transition-%d", i), int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimColumnReplacement(ctx, replacement.ID)
	if err != nil {
		t.Fatal(err)
	}
	partial, err := eng.stepColumnReplacement(ctx, replacement.ID, owner, 2)
	if err != nil {
		t.Fatal(err)
	}

	automatic := New(eng.store, catalog.New(eng.store), WithSchemaJobConfig(SchemaJobConfig{
		IndexBatchSize: 3, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	t.Cleanup(func() { _ = automatic.Close() })
	ready := waitForTransitionState(t, ctx, automatic, replacement.ID, model.TransitionReady)
	if ready.RowsScanned != 23 || ready.OwnerEpoch <= partial.OwnerEpoch {
		t.Fatalf("resumed replacement = %+v, partial=%+v", ready, partial)
	}

	table, age = replacementColumn(t, ctx, automatic, "users", "age")
	constraint, err := automatic.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
		Name: "users_age_required", Kind: model.ConstraintNotNull, ColumnID: age.SchemaID,
	})
	if err != nil {
		t.Fatal(err)
	}
	valid := waitForTransitionState(t, ctx, automatic, constraint.ID, model.TransitionReady)
	if valid.RowsScanned != 23 || valid.Constraint == nil ||
		valid.Constraint.State != model.ConstraintValid {
		t.Fatalf("automatic constraint validation = %+v", valid)
	}
}

func TestSchemaJobRunnerCloseAndFreshEngineResume(t *testing.T) {
	eng, ctx := setup(t)
	insertSchemaJobUsers(t, ctx, eng, 40, "restart")
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_restart", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}

	entered := make(chan struct{})
	var once sync.Once
	first := New(eng.store, catalog.New(eng.store),
		WithSchemaJobConfig(SchemaJobConfig{IndexBatchSize: 1, BatchesBeforeYield: 1}),
		withSchemaJobHooks(schemaJobHooks{afterBatch: func(ctx context.Context, kind schemaJobKind, id string) {
			if kind != schemaJobIndexBuild || id != transition.ID {
				return
			}
			once.Do(func() { close(entered) })
			<-ctx.Done()
		}}),
	)
	select {
	case <-entered:
	case <-time.After(5 * time.Second):
		t.Fatal("automatic worker did not commit its first batch")
	}
	closed := make(chan error, 1)
	go func() { closed <- first.Close() }()
	select {
	case err := <-closed:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("engine close did not cancel and join schema worker")
	}
	if err := first.Close(); err != nil {
		t.Fatalf("idempotent close: %v", err)
	}
	partial, err := first.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if partial.State != model.TransitionBuilding || partial.RowsScanned != 1 || len(partial.Cursor) == 0 {
		t.Fatalf("durable partial checkpoint = %+v", partial)
	}

	reopened := New(eng.store, catalog.New(eng.store), WithSchemaJobConfig(SchemaJobConfig{
		IndexBatchSize: 3, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	t.Cleanup(func() { _ = reopened.Close() })
	ready := waitForTransitionState(t, ctx, reopened, transition.ID, model.TransitionReady)
	if ready.RowsScanned != 40 || ready.OwnerEpoch <= partial.OwnerEpoch {
		t.Fatalf("resumed transition = %+v, partial owner=%d", ready, partial.OwnerEpoch)
	}
	assertIndexEqualsTable(t, ctx, reopened, "users", "users_age_restart")
}

func TestSchemaJobRunnerResumesAfterSlateCloseAndReopen(t *testing.T) {
	ctx := context.Background()
	databaseName := t.TempDir()
	firstStore, first := openFileEngine(t, databaseName, WithSchemaJobScheduling(false))
	if _, err := first.Catalog().CreateTable(ctx, model.TableDef{
		Name: "events",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "kind", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := first.Txn(ctx, func(tx *Tx) error {
		for i := range 17 {
			if err := tx.Insert(ctx, "events", lir.Row{
				"id": lir.Int64(int64(i)), "kind": lir.Text(fmt.Sprintf("kind-%d", i%3)),
			}); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	transition, err := first.startIndexBuild(ctx, "events", model.IndexDef{
		Name: "events_kind_online", Columns: []string{"kind"},
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := first.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	partial, err := first.stepIndexBuild(ctx, transition.ID, owner, 2)
	if err != nil {
		t.Fatal(err)
	}
	if partial.RowsScanned != 2 || len(partial.Cursor) == 0 {
		t.Fatalf("pre-close checkpoint = %+v", partial)
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	if err := firstStore.Close(); err != nil {
		t.Fatal(err)
	}

	_, reopened := openFileEngine(t, databaseName, WithSchemaJobConfig(SchemaJobConfig{
		IndexBatchSize: 2, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	ready := waitForTransitionState(t, ctx, reopened, transition.ID, model.TransitionReady)
	if ready.RowsScanned != 17 || ready.OwnerEpoch <= owner {
		t.Fatalf("post-reopen transition = %+v, old owner=%d", ready, owner)
	}
	assertIndexEqualsTable(t, ctx, reopened, "events", "events_kind_online")
}

func TestSchemaJobRunnerRoundRobinAcrossBuildAndReclamation(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	insertSchemaJobUsers(t, ctx, eng, 12, "fair")
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "retired_for_fairness",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Txn(ctx, func(tx *Tx) error {
		for i := range 12 {
			if err := tx.Insert(ctx, "retired_for_fairness", lir.Row{
				"id": lir.Int64(int64(i)), "value": lir.Text("retire"),
			}); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	retired, _, _ := eng.Catalog().GetTable(ctx, "retired_for_fairness")
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_fair", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().DeleteTable(ctx, "retired_for_fairness"); err != nil {
		t.Fatal(err)
	}

	events := make(chan schemaJobKind, 16)
	runner := New(eng.store, catalog.New(eng.store),
		WithSchemaJobConfig(SchemaJobConfig{
			IndexBatchSize: 1, ReclamationBatchSize: 1,
			BatchesBeforeYield: 2, YieldInterval: 10 * time.Millisecond,
		}),
		withSchemaJobHooks(schemaJobHooks{afterBatch: func(_ context.Context, kind schemaJobKind, _ string) {
			select {
			case events <- kind:
			default:
			}
		}}),
	)
	t.Cleanup(func() { _ = runner.Close() })
	seen := map[schemaJobKind]bool{}
	deadline := time.After(5 * time.Second)
	for !seen[schemaJobIndexBuild] || !seen[schemaJobReclamation] {
		select {
		case kind := <-events:
			seen[kind] = true
		case <-deadline:
			t.Fatalf("scheduler did not fairly serve both job classes: %v", seen)
		}
	}
	ready := waitForTransitionState(t, ctx, runner, transition.ID, model.TransitionReady)
	if ready.RowsScanned != 12 {
		t.Fatalf("ready transition = %+v", ready)
	}
	waitForReclamationState(t, ctx, eng.store, store.TableReclamationID(retired.ID), model.ReclamationReclaimed)
	assertRangeCount(t, ctx, eng.store, codec.DataPrefix(retired.ID), nil, 0)
}

func TestSchemaJobRunnerTakeoverFencesPreviouslyClaimedWorker(t *testing.T) {
	eng, ctx := setup(t)
	insertSchemaJobUsers(t, ctx, eng, 8, "takeover")
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_takeover", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}
	staleOwner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}

	automatic := New(eng.store, catalog.New(eng.store), WithSchemaJobConfig(SchemaJobConfig{
		IndexBatchSize: 1, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	t.Cleanup(func() { _ = automatic.Close() })
	ready := waitForTransitionState(t, ctx, automatic, transition.ID, model.TransitionReady)
	if ready.OwnerEpoch <= staleOwner {
		t.Fatalf("automatic owner = %d, stale owner = %d", ready.OwnerEpoch, staleOwner)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, staleOwner, 1); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale worker step = %v, want owner conflict", err)
	}
}

func TestIndexBuildDeltaBackpressureIsVisibleScopedAndRecoverable(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobConfig(SchemaJobConfig{
		DeltaSoftLimit: 2, DeltaHardLimit: 4,
	}))
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_pressure", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}
	for i := range 2 {
		if err := eng.Insert(ctx, "users", userRow(i, "pressure", int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	degraded, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if degraded.WorkState != model.TransitionWorkDegraded || degraded.DeltaHighWater-degraded.AppliedDelta != 2 {
		t.Fatalf("soft-limit state = %+v", degraded)
	}
	for i := 2; i < 4; i++ {
		if err := eng.Insert(ctx, "users", userRow(i, "pressure", int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	gated, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if gated.WorkState != model.TransitionWorkWriteGated || gated.DeltaHighWater-gated.AppliedDelta != 4 {
		t.Fatalf("hard-limit state = %+v", gated)
	}
	err = eng.Insert(ctx, "users", userRow(99, "blocked", 99))
	reason, marked := reject.ReasonOf(err)
	if !marked || reason != reject.ReasonTransitionBackpressure {
		t.Fatalf("hard-limit write error = %v reason=%q marked=%v", err, reason, marked)
	}
	if _, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(99)}); err != nil || ok {
		t.Fatalf("gated write leaked data: ok=%v err=%v", ok, err)
	}
	// The gate is table-scoped: unrelated foreground work remains admissible.
	if err := eng.Insert(ctx, "orders", lir.Row{
		"id": lir.Int64(1), "user_id": lir.Int64(0), "total": lir.Float64(1),
	}); err != nil {
		t.Fatalf("unrelated table was gated: %v", err)
	}
	ready, err := eng.runIndexBuild(ctx, transition.ID, 2)
	if err != nil {
		t.Fatal(err)
	}
	if ready.WorkState != model.TransitionWorkNormal || ready.AppliedDelta != ready.DeltaHighWater {
		t.Fatalf("ready transition retained pressure = %+v", ready)
	}
	if err := eng.Insert(ctx, "users", userRow(99, "resumed", 99)); err != nil {
		t.Fatalf("write did not resume after catch-up: %v", err)
	}
}

func TestSchemaJobRunnerQuarantinesBadJobWithoutStarvingOthers(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	insertSchemaJobUsers(t, ctx, eng, 1, "corrupt")
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_corrupt", Columns: []string{"age"},
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
	catchingUp, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1)
	if err != nil || catchingUp.State != model.TransitionCatchingUp {
		t.Fatalf("enter catch-up: state=%+v err=%v", catchingUp, err)
	}
	if err := eng.Insert(ctx, "users", userRow(9, "delta", 9)); err != nil {
		t.Fatal(err)
	}
	if err := eng.store.Put(ctx, store.DeltaKey(transition.ID, 1), []byte("{")); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name:       "quarantine_reclaim",
		Columns:    []model.ColumnDef{{Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "quarantine_reclaim", lir.Row{"id": lir.Int64(1)}); err != nil {
		t.Fatal(err)
	}
	retired, _, _ := eng.Catalog().GetTable(ctx, "quarantine_reclaim")
	if err := eng.Catalog().DeleteTable(ctx, "quarantine_reclaim"); err != nil {
		t.Fatal(err)
	}

	runner := New(eng.store, catalog.New(eng.store), WithSchemaJobConfig(SchemaJobConfig{
		IndexBatchSize: 1, ReclamationBatchSize: 1, BatchesBeforeYield: 2,
	}))
	t.Cleanup(func() { _ = runner.Close() })
	waitForReclamationState(t, ctx, eng.store, store.TableReclamationID(retired.ID), model.ReclamationReclaimed)
	failed, err := runner.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if failed.State != model.TransitionCatchingUp || failed.LastError == "" {
		t.Fatalf("bad job was not durably diagnosed and quarantined: %+v", failed)
	}
}

func TestSchemaJobRunnerCompactsCatalogHistoryInBoundedBatches(t *testing.T) {
	eng, ctx := setupWithOptions(t, withAutomaticReclamation(false))
	historyBatches := make(chan struct{}, 16)
	runner := New(
		eng.store,
		catalog.New(eng.store),
		WithSchemaJobConfig(SchemaJobConfig{
			CatalogHistoryRetain:    2,
			CatalogHistoryBatchSize: 1,
			BatchesBeforeYield:      1,
			IOBudgetItemsPerYield:   1,
			YieldInterval:           time.Millisecond,
		}),
		withSchemaJobHooks(schemaJobHooks{afterBatch: func(_ context.Context, kind schemaJobKind, _ string) {
			if kind == schemaJobCatalogHistory {
				select {
				case historyBatches <- struct{}{}:
				default:
				}
			}
		}}),
	)
	t.Cleanup(func() { _ = runner.Close() })
	for _, names := range [][2]string{
		{"users", "people"},
		{"people", "members"},
		{"members", "accounts"},
		{"accounts", "users"},
	} {
		if err := runner.Catalog().RenameTable(ctx, names[0], names[1]); err != nil {
			t.Fatal(err)
		}
	}

	deadline := time.Now().Add(5 * time.Second)
	for {
		revisions, err := runner.Catalog().Revisions(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if len(revisions) == 2 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("catalog history did not compact to two revisions: %+v", revisions)
		}
		time.Sleep(time.Millisecond)
	}
	hookDeadline := time.Now().Add(time.Second)
	for len(historyBatches) < 4 && time.Now().Before(hookDeadline) {
		time.Sleep(time.Millisecond)
	}
	if len(historyBatches) < 4 {
		t.Fatalf("history compaction used %d batches, want at least four single-item batches", len(historyBatches))
	}
	current, err := runner.Catalog().Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	compactedThrough, err := store.RevisionCompactedThrough(ctx, runner.store)
	if err != nil {
		t.Fatal(err)
	}
	if compactedThrough != current.Version-2 {
		t.Fatalf("history horizon = %d, current=%d retain=2", compactedThrough, current.Version)
	}
	metrics, err := runner.schemaStorageMetrics(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if metrics.CanonicalCatalogRevisions != 2 ||
		metrics.CatalogRevisionCompactedThrough != compactedThrough {
		t.Fatalf("catalog history metrics = %+v", metrics)
	}
}

func TestSchemaJobProgressRejectsRegressions(t *testing.T) {
	if got, err := schemaJobProgress("rows", 7, 9); err != nil || got != 2 {
		t.Fatalf("forward progress = %d, %v", got, err)
	}
	if _, err := schemaJobProgress("rows", 9, 7); err == nil {
		t.Fatal("accepted regressing durable progress")
	}
}

func insertSchemaJobUsers(t *testing.T, ctx context.Context, eng *Engine, count int, label string) {
	t.Helper()
	if err := eng.Txn(ctx, func(tx *Tx) error {
		for i := range count {
			if err := tx.Insert(ctx, "users", userRow(i, fmt.Sprintf("%s-%d", label, i), int64(i%11))); err != nil {
				return err
			}
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
}

func waitForTransitionState(t *testing.T, ctx context.Context, eng *Engine, id string, want model.TransitionState) model.SchemaTransition {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		transition, err := eng.inspectSchemaTransition(ctx, id)
		if err != nil {
			t.Fatal(err)
		}
		if transition.State == want {
			return transition
		}
		if transition.State == model.TransitionFailed || transition.State == model.TransitionCancelled || time.Now().After(deadline) {
			t.Fatalf("transition %s did not reach %s: %+v", id, want, transition)
		}
		time.Sleep(time.Millisecond)
	}
}

func waitForReclamationState(t *testing.T, ctx context.Context, view kv.KV, id string, want model.ReclamationState) model.Reclamation {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		reclamation, ok, err := store.GetReclamation(ctx, view, id)
		if err != nil {
			t.Fatal(err)
		}
		if ok && reclamation.State == want {
			return reclamation
		}
		if ok && reclamation.State == model.ReclamationFailed || time.Now().After(deadline) {
			t.Fatalf("reclamation %s did not reach %s: %+v", id, want, reclamation)
		}
		time.Sleep(time.Millisecond)
	}
}
