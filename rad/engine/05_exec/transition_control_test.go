package exec

import (
	"context"
	"strings"
	"testing"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestCancelSchemaTransitionStateContract(t *testing.T) {
	t.Run("unknown", func(t *testing.T) {
		eng, ctx := setup(t)
		if _, err := eng.CancelSchemaTransition(ctx, "tr-does-not-exist"); err == nil ||
			!strings.Contains(err.Error(), "does not exist") {
			t.Fatalf("unknown cancellation = %v", err)
		}
	})

	t.Run("cancelled is idempotent", func(t *testing.T) {
		eng, ctx := setup(t)
		started, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
			Name: "users_age_cancelled", Columns: []string{"age"},
		})
		if err != nil {
			t.Fatal(err)
		}
		first, err := eng.CancelSchemaTransition(ctx, started.ID)
		if err != nil {
			t.Fatal(err)
		}
		second, err := eng.CancelSchemaTransition(ctx, started.ID)
		if err != nil {
			t.Fatal(err)
		}
		if second.State != model.TransitionCancelled || second.Generation != first.Generation ||
			second.OwnerEpoch != first.OwnerEpoch {
			t.Fatalf("repeated cancellation changed transition:\nfirst:  %+v\nsecond: %+v", first, second)
		}
	})

	t.Run("ready", func(t *testing.T) {
		eng, ctx := setup(t)
		started, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
			Name: "users_age_ready", Columns: []string{"age"},
		})
		if err != nil {
			t.Fatal(err)
		}
		ready, err := eng.runIndexBuild(ctx, started.ID, 1)
		if err != nil {
			t.Fatal(err)
		}
		if ready.State != model.TransitionReady {
			t.Fatalf("transition = %+v, want ready", ready)
		}
		if _, err := eng.CancelSchemaTransition(ctx, started.ID); err == nil ||
			!strings.Contains(err.Error(), "not cancellable") {
			t.Fatalf("ready cancellation = %v", err)
		}
	})

	t.Run("failed", func(t *testing.T) {
		eng, ctx := setup(t)
		if err := eng.Insert(ctx, "users", lir.Row{
			"id": lir.Int64(1), "name": lir.Text("same"), "age": lir.Int64(1),
		}); err != nil {
			t.Fatal(err)
		}
		if err := eng.Insert(ctx, "users", lir.Row{
			"id": lir.Int64(2), "name": lir.Text("same"), "age": lir.Int64(2),
		}); err != nil {
			t.Fatal(err)
		}
		started, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
			Name: "users_name_failed", Columns: []string{"name"}, Unique: true,
		})
		if err != nil {
			t.Fatal(err)
		}
		owner, err := eng.claimIndexBuild(ctx, started.ID)
		if err != nil {
			t.Fatal(err)
		}
		failed := driveIndexBuildToTerminal(t, ctx, eng, started.ID, owner, 1)
		if failed.State != model.TransitionFailed {
			t.Fatalf("transition = %+v, want failed", failed)
		}
		if _, err := eng.CancelSchemaTransition(ctx, started.ID); err == nil ||
			!strings.Contains(err.Error(), "requires cleanup") {
			t.Fatalf("failed cancellation = %v", err)
		}
	})

	t.Run("unsupported kind", func(t *testing.T) {
		eng, ctx := setup(t)
		txn, err := eng.store.Begin(ctx, kv.SerializableSnapshot)
		if err != nil {
			t.Fatal(err)
		}
		defer txn.Rollback()
		now := time.Now().UTC()
		if err := store.SaveTransition(ctx, txn, model.SchemaTransition{
			ID: "tr-unsupported", Kind: model.TransitionKind("future_transition"),
			State: model.TransitionBuilding, Generation: 1,
			TableID: "future-table", TableSchemaID: 1,
			CreatedAt: now, UpdatedAt: now,
		}); err != nil {
			t.Fatal(err)
		}
		if err := txn.Commit(ctx); err != nil {
			t.Fatal(err)
		}
		if _, err := eng.CancelSchemaTransition(ctx, "tr-unsupported"); err == nil ||
			!strings.Contains(err.Error(), "does not support cancellation") {
			t.Fatalf("unsupported cancellation = %v", err)
		}
	})
}

func TestCancelSchemaTransitionReadyPublicationOrders(t *testing.T) {
	for _, tc := range []struct {
		name      string
		first     string
		wantState model.TransitionState
	}{
		{name: "cancel first", first: "cancel", wantState: model.TransitionCancelled},
		{name: "ready first", first: "ready", wantState: model.TransitionReady},
	} {
		t.Run(tc.name, func(t *testing.T) {
			eng, ctx := setup(t)
			transition, owner := prepareValidatingTransition(t, ctx, eng)

			if tc.first == "cancel" {
				if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err != nil {
					t.Fatal(err)
				}
				if _, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1); err == nil {
					t.Fatal("stale finalizer succeeded after cancellation")
				}
			} else {
				ready, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1)
				if err != nil {
					t.Fatal(err)
				}
				if ready.State != model.TransitionReady {
					t.Fatalf("finalization state = %q, want ready", ready.State)
				}
				if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err == nil ||
					!strings.Contains(err.Error(), "not cancellable") {
					t.Fatalf("cancellation after ready = %v", err)
				}
			}

			got, err := eng.inspectSchemaTransition(ctx, transition.ID)
			if err != nil {
				t.Fatal(err)
			}
			if got.State != tc.wantState {
				t.Fatalf("transition state = %q, want %q", got.State, tc.wantState)
			}
		})
	}
}

func TestCancelSchemaTransitionRacesReadyPublication(t *testing.T) {
	eng, ctx := setup(t)
	transition, owner := prepareValidatingTransition(t, ctx, eng)
	start := make(chan struct{})
	cancelResult := make(chan error, 1)
	readyResult := make(chan error, 1)

	go func() {
		<-start
		_, err := eng.CancelSchemaTransition(ctx, transition.ID)
		cancelResult <- err
	}()
	go func() {
		<-start
		_, err := eng.stepIndexBuild(ctx, transition.ID, owner, 1)
		readyResult <- err
	}()
	close(start)

	cancelErr, readyErr := <-cancelResult, <-readyResult
	got, err := eng.inspectSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	switch got.State {
	case model.TransitionCancelled:
		if cancelErr != nil || readyErr == nil {
			t.Fatalf("cancelled race errors: cancel=%v ready=%v", cancelErr, readyErr)
		}
	case model.TransitionReady:
		if readyErr != nil || cancelErr == nil {
			t.Fatalf("ready race errors: cancel=%v ready=%v", cancelErr, readyErr)
		}
	default:
		t.Fatalf("race left nonterminal state %q: cancel=%v ready=%v", got.State, cancelErr, readyErr)
	}
}

func prepareValidatingTransition(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
) (model.SchemaTransition, uint64) {
	t.Helper()
	started, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_age_race", Columns: []string{"age"},
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimIndexBuild(ctx, started.ID)
	if err != nil {
		t.Fatal(err)
	}
	validating := stepIndexBuildUntil(
		t, ctx, eng, started.ID, owner, 1, model.TransitionValidating,
	)
	return validating, owner
}
