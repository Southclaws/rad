package exec

import (
	"context"
	"errors"
	"fmt"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestOnlineIndexBuildConcurrentMutationDifferential(t *testing.T) {
	eng, ctx := setup(t)
	for i := range 32 {
		if err := eng.Insert(ctx, "users", userRow(i, fmt.Sprintf("base-%02d", i), int64(i%7))); err != nil {
			t.Fatal(err)
		}
	}

	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_online", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if transition.State != model.TransitionBuilding || transition.Index.State != model.IndexBuilding {
		t.Fatalf("start state = %+v", transition)
	}
	if _, err := eng.ScanIndex(ctx, "users", "users_age_online", nil); err == nil || !strings.Contains(err.Error(), "not ready") {
		t.Fatalf("building index was externally readable: %v", err)
	}
	schema, err := eng.Catalog().Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, index := range schema.Tables[0].Indexes {
		if index.Name == "users_age_online" {
			t.Fatal("building index leaked into canonical/planner-visible schema")
		}
	}

	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 3); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "users", userRow(100, "inserted", 42)); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := eng.Update(ctx, "users", lir.Row{"id": lir.Int64(1)}, lir.Row{"age": lir.Int64(99)}); err != nil || !ok {
		t.Fatalf("update: ok=%v err=%v", ok, err)
	}
	if ok, err := eng.Delete(ctx, "users", lir.Row{"id": lir.Int64(2)}); err != nil || !ok {
		t.Fatalf("delete: ok=%v err=%v", ok, err)
	}

	ready := driveIndexBuild(t, ctx, eng, transition.ID, owner, 4)
	if ready.State != model.TransitionReady || ready.Index.State != model.IndexReady {
		t.Fatalf("ready state = %+v", ready)
	}
	if ready.BasePosition == "" || ready.BarrierPosition == "" {
		t.Fatalf("transition did not retain opaque base/barrier positions: %+v", ready)
	}
	if ready.AppliedDelta != ready.DeltaHighWater || ready.DeltaHighWater != 4 {
		t.Fatalf("delta checkpoint = %d/%d, want 4/4", ready.AppliedDelta, ready.DeltaHighWater)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_age_online")

	rows, err := eng.ScanIndex(ctx, "users", "users_age_online", lir.Row{"age": lir.Int64(99)})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || !rows[0]["id"].Equal(lir.Int64(1)) {
		t.Fatalf("updated index lookup = %v", rows)
	}
}

func TestOnlineIndexBuildFencesWriterBoundBeforeCapture(t *testing.T) {
	eng, ctx := setup(t)
	oldWriter, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer oldWriter.Rollback()
	if err := oldWriter.Insert(ctx, "users", userRow(1, "old", 7)); err != nil {
		t.Fatal(err)
	}

	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_age_online", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	if err := oldWriter.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("pre-capture writer commit = %v, want conflict", err)
	}
	if _, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)}); err != nil || ok {
		t.Fatalf("conflicted row visible: ok=%v err=%v", ok, err)
	}

	if err := eng.Insert(ctx, "users", userRow(1, "retried", 7)); err != nil {
		t.Fatal(err)
	}
	ready, err := eng.runIndexBuild(ctx, transition.ID, 1)
	if err != nil {
		t.Fatal(err)
	}
	if ready.DeltaHighWater != 1 || ready.AppliedDelta != 1 {
		t.Fatalf("retried writer delta checkpoint = %d/%d", ready.AppliedDelta, ready.DeltaHighWater)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_age_online")
}

func TestIndexWorkerRejectsOtherTransitionKindsWithoutMutation(t *testing.T) {
	tests := []struct {
		name  string
		start func(*testing.T) (*Engine, context.Context, model.SchemaTransition)
		claim func(*testing.T, *Engine, context.Context, string) uint64
	}{
		{
			name: "column replacement",
			start: func(t *testing.T) (*Engine, context.Context, model.SchemaTransition) {
				eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false))
				table, column := replacementColumn(t, ctx, eng, "users", "age")
				transition, err := eng.startColumnReplacement(
					ctx,
					table.SchemaID,
					column.SchemaID,
					model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
				)
				if err != nil {
					t.Fatal(err)
				}
				return eng, ctx, transition
			},
			claim: func(t *testing.T, eng *Engine, ctx context.Context, id string) uint64 {
				owner, err := eng.claimColumnReplacement(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				return owner
			},
		},
		{
			name: "constraint validation",
			start: func(t *testing.T) (*Engine, context.Context, model.SchemaTransition) {
				eng, ctx := setupConstraintTable(t)
				table, column := replacementColumn(t, ctx, eng, "readings", "value")
				transition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
					Name: "readings_value_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
				})
				if err != nil {
					t.Fatal(err)
				}
				return eng, ctx, transition
			},
			claim: func(t *testing.T, eng *Engine, ctx context.Context, id string) uint64 {
				owner, err := eng.claimConstraintValidation(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				return owner
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			eng, ctx, transition := test.start(t)
			beforeClaim, err := eng.inspectSchemaTransition(ctx, transition.ID)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := eng.claimIndexBuild(ctx, transition.ID); err == nil ||
				!strings.Contains(err.Error(), "not an index build") {
				t.Fatalf("claim non-index transition = %v", err)
			}
			afterClaim, err := eng.inspectSchemaTransition(ctx, transition.ID)
			if err != nil {
				t.Fatal(err)
			}
			assertTransitionWorkerCheckpointUnchanged(t, beforeClaim, afterClaim)

			owner := test.claim(t, eng, ctx, transition.ID)
			beforeStep, err := eng.inspectSchemaTransition(ctx, transition.ID)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1); err == nil ||
				!strings.Contains(err.Error(), "not an index build") {
				t.Fatalf("step non-index transition = %v", err)
			}
			afterStep, err := eng.inspectSchemaTransition(ctx, transition.ID)
			if err != nil {
				t.Fatal(err)
			}
			assertTransitionWorkerCheckpointUnchanged(t, beforeStep, afterStep)
		})
	}
}

func assertTransitionWorkerCheckpointUnchanged(t *testing.T, before, after model.SchemaTransition) {
	t.Helper()
	if after.Kind != before.Kind || after.State != before.State ||
		after.Generation != before.Generation || after.OwnerEpoch != before.OwnerEpoch ||
		after.RowsScanned != before.RowsScanned || after.AppliedDelta != before.AppliedDelta {
		t.Fatalf("transition mutated by wrong worker: before=%+v after=%+v", before, after)
	}
}

func TestOnlineIndexBuildOwnerTakeoverAndFreshEngineResume(t *testing.T) {
	eng, ctx := setup(t)
	for i := range 11 {
		if err := eng.Insert(ctx, "users", userRow(i, fmt.Sprintf("u-%d", i), int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_age_online", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	ownerA, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	partial, err := eng.stepIndexBuild(ctx, transition.ID, ownerA, 2)
	if err != nil {
		t.Fatal(err)
	}
	if partial.RowsScanned != 2 || len(partial.Cursor) == 0 {
		t.Fatalf("partial checkpoint = %+v", partial)
	}

	reopened := New(eng.store, catalog.New(eng.store), withAutomaticIndexBuilds(false))
	t.Cleanup(func() { _ = reopened.Close() })
	ownerB, err := reopened.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if ownerB <= ownerA {
		t.Fatalf("takeover epoch = %d, old = %d", ownerB, ownerA)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, ownerA, 2); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale owner step = %v, want conflict", err)
	}

	ready := driveIndexBuild(t, ctx, reopened, transition.ID, ownerB, 2)
	if ready.RowsScanned != 11 {
		t.Fatalf("resumed rows scanned = %d, want 11", ready.RowsScanned)
	}
	assertIndexEqualsTable(t, ctx, reopened, "users", "users_age_online")
}

func TestOnlineIndexBuildCancellationIsAtomicAndFencesWorker(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "before", 1)); err != nil {
		t.Fatal(err)
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
	if err := eng.Insert(ctx, "users", userRow(2, "captured", 2)); err != nil {
		t.Fatal(err)
	}
	beforeCancel, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}

	cancelled, err := eng.CancelSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != model.TransitionCancelled || cancelled.OwnerEpoch <= owner {
		t.Fatalf("cancelled transition = %+v", cancelled)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("cancelled worker step = %v, want ownership conflict", err)
	}
	if err := eng.Insert(ctx, "users", userRow(3, "after", 3)); err != nil {
		t.Fatal(err)
	}
	afterCancel, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if afterCancel.DeltaHighWater != beforeCancel.DeltaHighWater {
		t.Fatalf("delta capture continued after cancel: %d -> %d", beforeCancel.DeltaHighWater, afterCancel.DeltaHighWater)
	}
	if _, err := eng.ScanIndex(ctx, "users", "users_age_online", nil); err == nil || !strings.Contains(err.Error(), "no index") {
		t.Fatalf("cancelled index remained visible: %v", err)
	}
	table, ok, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("get table: ok=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, table)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.DeltaSinks) != 0 {
		t.Fatalf("cancelled transition remains in write protocol: %+v", protocol.DeltaSinks)
	}
}

func TestOnlineIndexBuildConcurrentTraffic(t *testing.T) {
	eng, ctx := setup(t)
	for i := range 40 {
		if err := eng.Insert(ctx, "users", userRow(i, fmt.Sprintf("base-%d", i), int64(i%5))); err != nil {
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

	errCh := make(chan error, 8)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := 0; i < 10_000; i++ {
			state, err := eng.stepIndexBuild(ctx, transition.ID, owner, 3)
			if errors.Is(err, kv.ErrConflict) {
				runtime.Gosched()
				continue
			}
			if err != nil {
				errCh <- err
				return
			}
			if state.State == model.TransitionReady {
				return
			}
			runtime.Gosched()
		}
		errCh <- errors.New("online index worker made no bounded progress")
	}()

	for worker := range 4 {
		wg.Add(1)
		go func(worker int) {
			defer wg.Done()
			for action := range 16 {
				id := 1_000 + worker*100 + action
				err := retryConflicts(func() error {
					return eng.Insert(ctx, "users", userRow(id, fmt.Sprintf("w%d-%d", worker, action), int64((worker+action)%9)))
				})
				if err != nil {
					errCh <- err
					return
				}
				if action%3 == 0 {
					err = retryConflicts(func() error {
						_, ok, err := eng.Update(ctx, "users", lir.Row{"id": lir.Int64(int64(id))}, lir.Row{"age": lir.Int64(77)})
						if err == nil && !ok {
							return errors.New("concurrent update lost its target")
						}
						return err
					})
					if err != nil {
						errCh <- err
						return
					}
				}
				if action%5 == 0 {
					err = retryConflicts(func() error {
						ok, err := eng.Delete(ctx, "users", lir.Row{"id": lir.Int64(int64(id))})
						if err == nil && !ok {
							return errors.New("concurrent delete lost its target")
						}
						return err
					})
					if err != nil {
						errCh <- err
						return
					}
				}
			}
		}(worker)
	}
	wg.Wait()
	close(errCh)
	for err := range errCh {
		t.Error(err)
	}
	if t.Failed() {
		return
	}
	state, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if state.State != model.TransitionReady {
		t.Fatalf("transition stopped in %q", state.State)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_age_online")
}

func TestOnlineIndexBuildAdmissionRules(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	first, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_age_online", Columns: []string{"age"}})
	if err != nil {
		t.Fatal(err)
	}
	second, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_id_online", Columns: []string{"id"}})
	if err != nil {
		t.Fatalf("compatible second transition admission: %v", err)
	}
	table, ok, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("users table: found=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, table)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.DeltaSinks) != 2 ||
		protocol.DeltaSinks[0].TransitionID > protocol.DeltaSinks[1].TransitionID {
		t.Fatalf("composed delta sinks are not canonical: %+v", protocol.DeltaSinks)
	}
	unique, err := eng.startIndexBuild(ctx, "orders", model.IndexDef{Name: "orders_total_unique_online", Columns: []string{"total"}, Unique: true})
	if err != nil {
		t.Fatalf("online unique admission: %v", err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, unique.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, first.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, second.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.startIndexBuild(ctx, "users", model.IndexDef{Name: "users_id_online", Columns: []string{"id"}}); err != nil {
		t.Fatalf("new build after cancellation: %v", err)
	}
}

func TestOnlineUniqueIndexBuildTracksConcurrentMutationsAndEnforcesAfterPublish(t *testing.T) {
	eng, ctx := setup(t)
	for i := range 12 {
		if err := eng.Insert(ctx, "users", userRow(i, fmt.Sprintf("name-%02d", i), int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_unique_online", Columns: []string{"name"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 2); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "users", userRow(100, "inserted", 100)); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := eng.Update(ctx, "users", lir.Row{"id": lir.Int64(1)}, lir.Row{"name": lir.Text("renamed")}); err != nil || !ok {
		t.Fatalf("update: ok=%v err=%v", ok, err)
	}
	if ok, err := eng.Delete(ctx, "users", lir.Row{"id": lir.Int64(2)}); err != nil || !ok {
		t.Fatalf("delete: ok=%v err=%v", ok, err)
	}
	ready := driveIndexBuild(t, ctx, eng, transition.ID, owner, 2)
	if ready.State != model.TransitionReady || !ready.Index.Unique {
		t.Fatalf("ready transition = %+v", ready)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_name_unique_online")
	err = eng.Insert(ctx, "users", userRow(101, "inserted", 101))
	reason, ok := reject.ReasonOf(err)
	if err == nil || !ok || reason != reject.ReasonConstraintViolation {
		t.Fatalf("duplicate after publication = %v, reason=%q", err, reason)
	}
}

func TestOnlineUniqueIndexBuildFailsDurablyOnDuplicateAndReleasesGate(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "duplicate", 1)); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "users", userRow(2, "duplicate", 2)); err != nil {
		t.Fatal(err)
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_unique_online", Columns: []string{"name"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	failed := driveIndexBuildToTerminal(t, ctx, eng, transition.ID, owner, 1)
	if failed.State != model.TransitionFailed || !strings.Contains(failed.LastError, "shared by multiple rows") {
		t.Fatalf("failed transition = %+v", failed)
	}
	if err := eng.Insert(ctx, "users", userRow(3, "after-failure", 3)); err != nil {
		t.Fatalf("write gate remained after failure: %v", err)
	}
	if _, err := eng.ScanIndex(ctx, "users", "users_name_unique_online", nil); err == nil || !strings.Contains(err.Error(), "no index") {
		t.Fatalf("failed index definition remained bindable: %v", err)
	}
}

func TestOnlineUniqueIndexBuildAllowsDuplicateResolvedBeforeValidation(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "one", 1)); err != nil {
		t.Fatal(err)
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_unique_online", Columns: []string{"name"}, Unique: true,
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
	if err := eng.Insert(ctx, "users", userRow(2, "one", 2)); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := eng.Update(ctx, "users", lir.Row{"id": lir.Int64(2)}, lir.Row{"name": lir.Text("two")}); err != nil || !ok {
		t.Fatalf("resolve duplicate: ok=%v err=%v", ok, err)
	}
	ready := driveIndexBuild(t, ctx, eng, transition.ID, owner, 1)
	if ready.State != model.TransitionReady {
		t.Fatalf("resolved build = %+v", ready)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_name_unique_online")
}

func TestOnlineUniqueIndexBuildTreatsNullsAsDistinct(t *testing.T) {
	eng, ctx := setup(t)
	for i := range 4 {
		row := userRow(i+1, fmt.Sprintf("nullable-%d", i), int64(i))
		row["age"] = lir.Null(model.TypeInt64)
		if err := eng.Insert(ctx, "users", row); err != nil {
			t.Fatal(err)
		}
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_unique_online", Columns: []string{"age"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	ready := driveIndexBuild(t, ctx, eng, transition.ID, owner, 1)
	if ready.State != model.TransitionReady {
		t.Fatalf("nullable unique build = %+v", ready)
	}
	row := userRow(10, "another-null", 0)
	row["age"] = lir.Null(model.TypeInt64)
	if err := eng.Insert(ctx, "users", row); err != nil {
		t.Fatalf("ready unique index rejected another NULL: %v", err)
	}
}

func TestOnlineUniqueIndexFinalizationGateIsScopedFencedAndBounded(t *testing.T) {
	eng, ctx := setup(t)
	for i := range 5 {
		if err := eng.Insert(ctx, "users", userRow(i+1, fmt.Sprintf("name-%d", i), int64(i))); err != nil {
			t.Fatal(err)
		}
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_unique_online", Columns: []string{"name"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	oldWriter, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer oldWriter.Rollback()
	if _, ok, err := oldWriter.Update(ctx, "users", lir.Row{"id": lir.Int64(1)}, lir.Row{"age": lir.Int64(99)}); err != nil || !ok {
		t.Fatalf("old writer update: ok=%v err=%v", ok, err)
	}
	stepIndexBuildUntil(t, ctx, eng, transition.ID, owner, 1, model.TransitionValidating)
	if err := oldWriter.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("writer admitted before gate committed with %v, want conflict", err)
	}
	err = eng.Insert(ctx, "users", userRow(20, "gated", 20))
	reason, ok := reject.ReasonOf(err)
	if err == nil || !ok || reason != reject.ReasonTransitionFinalizing {
		t.Fatalf("gated write = %v, reason=%q", err, reason)
	}
	if err := eng.Insert(ctx, "orders", lir.Row{
		"id": lir.Int64(1), "user_id": lir.Int64(1), "total": lir.Float64(10),
	}); err != nil {
		t.Fatalf("unrelated table was gated: %v", err)
	}
	newOwner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale validating owner = %v, want conflict", err)
	}
	ready := driveIndexBuild(t, ctx, eng, transition.ID, newOwner, 1)
	if ready.BarrierPosition == "" {
		t.Fatal("ready transition lacks its final barrier position")
	}
	if err := eng.Insert(ctx, "users", userRow(20, "after-gate", 20)); err != nil {
		t.Fatalf("write gate remained after publication: %v", err)
	}
}

func TestOnlineUniqueIndexExposesSemanticWorkerYieldPoints(t *testing.T) {
	var events []YieldEvent
	eng, ctx := setupWithOptions(t, WithYieldHook(func(_ context.Context, event YieldEvent) {
		events = append(events, event)
	}))
	ctx = WithYieldActor(ctx, "unique-builder")
	if err := eng.Insert(ctx, "users", userRow(1, "one", 1)); err != nil {
		t.Fatal(err)
	}
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_unique_online", Columns: []string{"name"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	driveIndexBuild(t, ctx, eng, transition.ID, owner, 1)
	want := map[YieldPoint]bool{
		YieldOwnerTakeover:            false,
		YieldTransitionBatchIntent:    false,
		YieldTransitionCheckpoint:     false,
		YieldFinalizationGateAcquired: false,
	}
	for _, event := range events {
		if _, tracked := want[event.Point]; tracked && event.Entity == transition.ID && event.Actor == "unique-builder" {
			want[event.Point] = true
		}
	}
	for point, seen := range want {
		if !seen {
			t.Errorf("yield point %q was not observed; events=%+v", point, events)
		}
	}
}

func TestOnlineIndexBuildMatchesSynchronousOracle(t *testing.T) {
	eng, ctx := setup(t)
	for _, table := range []string{"online_items", "sync_items"} {
		if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
			Name: table,
			Columns: []model.ColumnDef{
				{Name: "id", Type: model.TypeInt64},
				{Name: "value", Type: model.TypeText},
			},
			PrimaryKey: []string{"id"},
		}); err != nil {
			t.Fatal(err)
		}
		for i := range 17 {
			if err := eng.Insert(ctx, table, lir.Row{"id": lir.Int64(int64(i)), "value": lir.Text(fmt.Sprintf("v-%d", i%4))}); err != nil {
				t.Fatal(err)
			}
		}
	}

	transition, err := eng.startIndexBuild(ctx, "online_items", model.IndexDef{Name: "online_value_idx", Columns: []string{"value"}})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 2); err != nil {
		t.Fatal(err)
	}
	for _, table := range []string{"online_items", "sync_items"} {
		if err := eng.Insert(ctx, table, lir.Row{"id": lir.Int64(100), "value": lir.Text("inserted")}); err != nil {
			t.Fatal(err)
		}
		if _, ok, err := eng.Update(ctx, table, lir.Row{"id": lir.Int64(1)}, lir.Row{"value": lir.Text("updated")}); err != nil || !ok {
			t.Fatalf("update %s: ok=%v err=%v", table, ok, err)
		}
		if ok, err := eng.Delete(ctx, table, lir.Row{"id": lir.Int64(2)}); err != nil || !ok {
			t.Fatalf("delete %s: ok=%v err=%v", table, ok, err)
		}
	}
	if err := eng.CreateIndexWithBackfill(ctx, "sync_items", model.IndexDef{Name: "sync_value_idx", Columns: []string{"value"}}); err != nil {
		t.Fatal(err)
	}
	driveIndexBuild(t, ctx, eng, transition.ID, owner, 2)

	onlineRows, err := eng.ScanIndex(ctx, "online_items", "online_value_idx", nil)
	if err != nil {
		t.Fatal(err)
	}
	syncRows, err := eng.ScanIndex(ctx, "sync_items", "sync_value_idx", nil)
	if err != nil {
		t.Fatal(err)
	}
	shape := func(rows []lir.Row) []string {
		out := make([]string, len(rows))
		for i, row := range rows {
			out[i] = fmt.Sprintf("%d:%s", row["id"].Int64, row["value"].Text)
		}
		slices.Sort(out)
		return out
	}
	if got, want := shape(onlineRows), shape(syncRows); !slices.Equal(got, want) {
		t.Fatalf("online index != synchronous oracle: online=%v sync=%v", got, want)
	}
}

func driveIndexBuild(t *testing.T, ctx context.Context, eng *Engine, transitionID string, owner uint64, batchSize int) model.SchemaTransition {
	t.Helper()
	for i := 0; i < 10_000; i++ {
		transition, err := eng.stepIndexBuild(ctx, transitionID, owner, batchSize)
		if errors.Is(err, kv.ErrConflict) {
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		if transition.State == model.TransitionReady {
			return transition
		}
	}
	t.Fatal("online index build made no bounded progress")
	return model.SchemaTransition{}
}

func driveIndexBuildToTerminal(t *testing.T, ctx context.Context, eng *Engine, transitionID string, owner uint64, batchSize int) model.SchemaTransition {
	t.Helper()
	for i := 0; i < 10_000; i++ {
		transition, err := eng.stepIndexBuild(ctx, transitionID, owner, batchSize)
		if errors.Is(err, kv.ErrConflict) {
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		if transition.State == model.TransitionReady || transition.State == model.TransitionFailed {
			return transition
		}
	}
	t.Fatal("online index build made no bounded progress")
	return model.SchemaTransition{}
}

func stepIndexBuildUntil(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
	transitionID string,
	owner uint64,
	batchSize int,
	want model.TransitionState,
) model.SchemaTransition {
	t.Helper()
	for i := 0; i < 10_000; i++ {
		transition, err := eng.stepIndexBuild(ctx, transitionID, owner, batchSize)
		if errors.Is(err, kv.ErrConflict) {
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		if transition.State == want {
			return transition
		}
		if transition.State == model.TransitionReady || transition.State == model.TransitionFailed {
			t.Fatalf("transition reached terminal state %q before %q", transition.State, want)
		}
	}
	t.Fatalf("online index build never reached %q", want)
	return model.SchemaTransition{}
}

func retryConflicts(fn func() error) error {
	for i := 0; i < 1_000; i++ {
		err := fn()
		if !errors.Is(err, kv.ErrConflict) {
			return err
		}
		runtime.Gosched()
	}
	return errors.New("transaction made no progress after conflict retries")
}

func userRow(id int, name string, age int64) lir.Row {
	return lir.Row{"id": lir.Int64(int64(id)), "name": lir.Text(name), "age": lir.Int64(age)}
}

func assertIndexEqualsTable(t *testing.T, ctx context.Context, eng *Engine, table, index string) {
	t.Helper()
	indexed, err := eng.ScanIndex(ctx, table, index, nil)
	if err != nil {
		t.Fatal(err)
	}
	it, err := eng.ScanTable(ctx, table)
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	var scanned []lir.Row
	for {
		row, ok, err := it.Next()
		if err != nil {
			t.Fatal(err)
		}
		if !ok {
			break
		}
		scanned = append(scanned, row)
	}
	ids := func(rows []lir.Row) []int64 {
		out := make([]int64, len(rows))
		for i, row := range rows {
			out[i] = row["id"].Int64
		}
		slices.Sort(out)
		return out
	}
	if got, want := ids(indexed), ids(scanned); !slices.Equal(got, want) {
		t.Fatalf("index/table row identities differ: index=%v table=%v", got, want)
	}
}
