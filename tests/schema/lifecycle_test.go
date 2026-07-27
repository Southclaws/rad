package schema_test

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"sync"
	"testing"
	"time"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

const twoValueColumns = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id,          type: int64,  pk: true }
      - { id: 2, name: left_value,  type: string }
      - { id: 3, name: right_value, type: string }
`

const replacedValueColumns = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id,          type: int64, pk: true }
      - { id: 2, name: left_value,  type: int64 }
      - { id: 3, name: right_value, type: int64 }
    indexes:
      - { name: events_value_pair_uq, columns: [left_value, right_value], unique: true }
`

func TestDependentMigrationGraphConvergesAcrossConcurrentWrites(t *testing.T) {
	ctx := t.Context()
	db := newDatabase(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize:        32,
		BatchesBeforeYield:    3,
		IOBudgetItemsPerYield: 96,
		YieldInterval:         time.Millisecond,
	}))
	db.SchemaMigrateReady(twoValueColumns, false)

	const initialRows = 96
	seed := make([]map[string]any, initialRows)
	for id := 1; id <= initialRows; id++ {
		seed[id-1] = map[string]any{
			"id": id, "left_value": fmt.Sprint(id), "right_value": fmt.Sprint(id + 10_000),
		}
	}
	db.Insert("events", seed...)

	started := db.SchemaMigrate(replacedValueColumns, false)
	if started.State != protocol.SchemaMigrationConverging || len(started.TransitionIDs) != 3 {
		t.Fatalf("started migration = %#v, want three-node converging graph", started)
	}
	graph := transitionGraph(t, db.Client, started.TransitionIDs)
	replacements := transitionIDsByKind(graph, radclient.TransitionColumnReplacement)
	indexes := transitionIDsByKind(graph, radclient.TransitionIndexBuild)
	if len(replacements) != 2 || len(indexes) != 1 {
		t.Fatalf("transition kinds: replacements=%v indexes=%v graph=%#v", replacements, indexes, graph)
	}
	prerequisites := slices.Clone(graph[indexes[0]].Prerequisites)
	slices.Sort(prerequisites)
	if !slices.Equal(prerequisites, replacements) {
		t.Fatalf("index prerequisites = %v, want both replacements %v", prerequisites, replacements)
	}

	const writers = 6
	const rowsPerWriter = 18
	errCh := make(chan error, writers)
	var wait sync.WaitGroup
	for writer := range writers {
		client, err := radclient.Dial(db.URL)
		if err != nil {
			t.Fatal(err)
		}
		wait.Add(1)
		go func(writer int, client *radclient.Client) {
			defer wait.Done()
			for offset := range rowsPerWriter {
				id := initialRows + 1 + writer*rowsPerWriter + offset
				if err := createAcrossPublication(
					ctx,
					client,
					"events",
					map[string]any{"id": id},
					map[string]any{"id": id, "left_value": fmt.Sprint(id), "right_value": fmt.Sprint(id + 10_000)},
					map[string]any{"id": id, "left_value": id, "right_value": id + 10_000},
				); err != nil {
					errCh <- fmt.Errorf("writer %d row %d: %w", writer, id, err)
					return
				}
			}
		}(writer, client)
	}
	wait.Wait()
	close(errCh)
	for err := range errCh {
		t.Error(err)
	}
	if t.Failed() {
		return
	}

	waitContext, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	ready, err := db.Client.WaitSchemaMigration(waitContext, started)
	if err != nil {
		controls, inspectErr := db.Client.SchemaTransitions(ctx)
		t.Fatalf("wait for migration: %v; transitions=%#v; inspect=%v", err, controls, inspectErr)
	}
	if ready.State != protocol.SchemaMigrationReady || ready.SchemaHash != ready.DesiredHash {
		t.Fatalf("ready migration = %#v", ready)
	}

	rows, err := db.Client.Query(ctx, lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"e":       lirwire.Scan("events", "e"),
			"ordered": lirwire.Order("e", []lirwire.OrderTerm{{Expr: lirwire.Col("e", "id")}}),
		},
		Root: lirwire.Root{Node: "ordered", Cardinality: "many"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if want := initialRows + writers*rowsPerWriter; len(rows) != want {
		t.Fatalf("row count = %d, want %d", len(rows), want)
	}
	for _, row := range rows {
		if _, ok := row["left_value"].(json.Number); !ok {
			t.Fatalf("left_value retained pre-migration representation: %T (%v)", row["left_value"], row["left_value"])
		}
		if _, ok := row["right_value"].(json.Number); !ok {
			t.Fatalf("right_value retained pre-migration representation: %T (%v)", row["right_value"], row["right_value"])
		}
	}
	if _, err := db.Client.Create(ctx, "events", map[string]any{
		"id": 1_000_000, "left_value": 1, "right_value": 10_001,
	}); err == nil {
		t.Fatal("published composite unique index accepted a duplicate pair")
	}
}
