package concurrent

import (
	"context"
	"sync"
	"testing"
	"time"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestExternalCancellationRacesAutomaticIndexWorker(t *testing.T) {
	workerReachedBatch := make(chan struct{})
	releaseWorker := make(chan struct{})
	var reachOnce sync.Once
	var releaseOnce sync.Once
	release := func() { releaseOnce.Do(func() { close(releaseWorker) }) }
	t.Cleanup(release)

	db := newChaosDB(
		t,
		exec.WithSchemaJobConfig(exec.SchemaJobConfig{
			IndexBatchSize: 1, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
		}),
		exec.WithYieldHook(func(ctx context.Context, event exec.YieldEvent) {
			if event.Point != exec.YieldTransitionBatchIntent {
				return
			}
			reachOnce.Do(func() {
				close(workerReachedBatch)
				select {
				case <-releaseWorker:
				case <-ctx.Done():
				}
			})
		}),
	)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	tableID, idColumn, valueColumn := pirwire.SchemaID(104), pirwire.SchemaID(1), pirwire.SchemaID(2)
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.CreateTable(
		"create",
		pirwire.TableDefinition{
			ID: &tableID, Name: "cancelled_build_values",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
			},
			PrimaryKey: []string{"id"},
		},
	))); err != nil {
		t.Fatal(err)
	}
	if err := seedTransitionRows(ctx, db.Control, "cancelled_build_values", lirwire.ScalarTypeText, 64); err != nil {
		t.Fatal(err)
	}
	started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartIndexBuild(
		"build",
		tableID,
		pirwire.IndexDefinition{Name: "cancelled_value_online", Columns: []string{"value"}},
	)))
	if err != nil {
		t.Fatal(err)
	}
	if len(started.Statements) != 1 || started.Statements[0].Control == nil {
		t.Fatalf("start index = %+v", started.Statements)
	}
	transitionID := started.Statements[0].Control.TransitionID

	select {
	case <-workerReachedBatch:
	case <-ctx.Done():
		t.Fatalf("automatic worker did not reach a bounded batch: %v", ctx.Err())
	}
	cancelled, err := db.Control.CancelSchemaTransition(ctx, transitionID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != radclient.TransitionCancelled {
		t.Fatalf("cancelled transition = %+v", cancelled)
	}
	release()

	for observationDeadline := time.Now().Add(100 * time.Millisecond); time.Now().Before(observationDeadline); {
		transition, err := db.Control.SchemaTransition(ctx, transitionID)
		if err != nil {
			t.Fatal(err)
		}
		if transition.State != radclient.TransitionCancelled {
			t.Fatalf("stale automatic worker changed cancelled transition: %+v", transition)
		}
		time.Sleep(10 * time.Millisecond)
	}

	table, ok, err := db.Catalog.GetTable(ctx, "cancelled_build_values")
	if err != nil || !ok {
		t.Fatalf("cancelled build table: found=%v err=%v", ok, err)
	}
	for _, index := range table.Indexes {
		if index.Name == "cancelled_value_online" {
			t.Fatalf("cancelled index remained in table definition: %+v", index)
		}
	}
	if _, err := db.Control.Create(ctx, table.Name, map[string]any{
		"id": int64(1_000_000), "value": "after-cancel",
	}); err != nil {
		t.Fatalf("cancelled build retained a foreground write obligation: %v", err)
	}
}
