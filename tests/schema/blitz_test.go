package schema_test

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"runtime"
	"slices"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

const blitzInitialSchema = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id,       type: int64, pk: true }
      - { id: 2, name: value,    type: string }
      - { id: 3, name: revision, type: int64 }
`

const blitzIndexedSchema = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id,       type: int64, pk: true }
      - { id: 2, name: value,    type: string }
      - { id: 3, name: revision, type: int64 }
      - { id: 4, name: note,     type: string, nullable: true }
    indexes:
      - { name: events_revision_idx, columns: [revision] }
`

const blitzReplacedSchema = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id,       type: int64, pk: true }
      - { id: 2, name: value,    type: int64 }
      - { id: 3, name: revision, type: int64 }
      - { id: 4, name: note,     type: string, nullable: true }
    indexes:
      - { name: events_revision_idx, columns: [revision] }
      - { name: events_value_uq, columns: [value], unique: true }
`

func TestConcurrentSchemaChangesDuringReadWriteBlitz(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 40*time.Second)
	defer cancel()
	db := newDatabase(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize:        16,
		BatchesBeforeYield:    3,
		IOBudgetItemsPerYield: 64,
		YieldInterval:         time.Millisecond,
	}))
	db.SchemaMigrateReady(blitzInitialSchema, false)

	const initialRows = 128
	seed := make([]map[string]any, initialRows)
	for id := 1; id <= initialRows; id++ {
		seed[id-1] = map[string]any{"id": id, "value": fmt.Sprint(id), "revision": 0}
	}
	db.Insert("events", seed...)

	trafficCtx, stopTraffic := context.WithCancel(ctx)
	defer stopTraffic()
	start := make(chan struct{})
	errorsCh := make(chan error, 16)
	var reportOnce sync.Once
	report := func(err error) {
		reportOnce.Do(func() {
			errorsCh <- err
			cancel()
		})
	}

	var activeStage atomic.Int32
	var readsDuring [3]atomic.Uint64
	var writesDuring [3]atomic.Uint64
	var totalReads atomic.Uint64
	var totalWrites atomic.Uint64

	const writers = 6
	const rowsPerWriter = 20
	var writerGroup sync.WaitGroup
	for writer := range writers {
		client, err := radclient.Dial(db.URL)
		if err != nil {
			t.Fatal(err)
		}
		writerGroup.Add(1)
		go func(writer int, client *radclient.Client) {
			defer writerGroup.Done()
			<-start
			for offset := range rowsPerWriter {
				id := initialRows + 1 + writer*rowsPerWriter + offset
				if err := createAcrossPublication(
					trafficCtx,
					client,
					"events",
					map[string]any{"id": id},
					map[string]any{"id": id, "value": fmt.Sprint(id), "revision": 0},
					map[string]any{"id": id, "value": id, "revision": 0},
				); err != nil {
					report(fmt.Errorf("writer %d create row %d: %w", writer, id, err))
					return
				}
				recordWrite(&activeStage, &totalWrites, &writesDuring)
				if err := updateBlitzRevision(trafficCtx, client, id, int64(offset+1)); err != nil {
					report(fmt.Errorf("writer %d update row %d: %w", writer, id, err))
					return
				}
				recordWrite(&activeStage, &totalWrites, &writesDuring)
				runtime.Gosched()
			}
		}(writer, client)
	}

	const readers = 4
	var readerGroup sync.WaitGroup
	for reader := range readers {
		client, err := radclient.Dial(db.URL)
		if err != nil {
			t.Fatal(err)
		}
		readerGroup.Add(1)
		go func(reader int, client *radclient.Client) {
			defer readerGroup.Done()
			<-start
			for {
				select {
				case <-trafficCtx.Done():
					return
				default:
				}
				var err error
				if reader%2 == 0 {
					err = pointReadBlitzRow(trafficCtx, client, int64(reader+1))
				} else {
					err = scanBlitzRows(trafficCtx, client, initialRows)
				}
				if err != nil {
					if trafficCtx.Err() != nil {
						return
					}
					if radclient.IsConflict(err) {
						runtime.Gosched()
						continue
					}
					report(fmt.Errorf("reader %d: %w", reader, err))
					return
				}
				totalReads.Add(1)
				if stage := activeStage.Load(); stage > 0 {
					readsDuring[stage].Add(1)
				}
				select {
				case <-trafficCtx.Done():
					return
				case <-time.After(2 * time.Millisecond):
				}
			}
		}(reader, client)
	}
	close(start)

	activeStage.Store(1)
	indexed, indexedRequests, err := applySchemaStageConcurrently(ctx, db.Client, db.URL, blitzIndexedSchema, 4)
	if err == nil && (indexed.State != protocol.SchemaMigrationReady || indexed.SchemaHash != indexed.DesiredHash) {
		err = fmt.Errorf("indexed stage did not converge: %#v", indexed)
	}
	if err != nil {
		report(fmt.Errorf("indexed schema stage: %w", err))
	}

	if ctx.Err() == nil {
		activeStage.Store(2)
		replaced, replacedRequests, stageErr := applySchemaStageConcurrently(ctx, db.Client, db.URL, blitzReplacedSchema, 4)
		if stageErr == nil && (replaced.State != protocol.SchemaMigrationReady || replaced.SchemaHash != replaced.DesiredHash) {
			stageErr = fmt.Errorf("replacement stage did not converge: %#v", replaced)
		}
		if stageErr != nil {
			report(fmt.Errorf("replacement schema stage: %w", stageErr))
		}
		indexedRequests += replacedRequests
	}
	activeStage.Store(0)

	writerGroup.Wait()
	stopTraffic()
	readerGroup.Wait()
	close(errorsCh)
	for err := range errorsCh {
		t.Error(err)
	}
	if t.Failed() {
		return
	}

	if indexedRequests != 8 {
		t.Fatalf("successful schema requests = %d, want 8", indexedRequests)
	}
	for stage := int32(1); stage <= 2; stage++ {
		if readsDuring[stage].Load() == 0 || writesDuring[stage].Load() == 0 {
			t.Fatalf(
				"stage %d did not overlap both traffic classes: reads=%d writes=%d",
				stage,
				readsDuring[stage].Load(),
				writesDuring[stage].Load(),
			)
		}
	}
	if totalReads.Load() < 20 || totalWrites.Load() != writers*rowsPerWriter*2 {
		t.Fatalf("blitz coverage: reads=%d writes=%d", totalReads.Load(), totalWrites.Load())
	}

	rows, err := db.Client.Query(ctx, blitzScanQuery())
	if err != nil {
		t.Fatal(err)
	}
	if want := initialRows + writers*rowsPerWriter; len(rows) != want {
		t.Fatalf("final row count = %d, want %d", len(rows), want)
	}
	for i, row := range rows {
		if err := validateBlitzRecord(row, true); err != nil {
			t.Fatalf("final row %d: %v", i, err)
		}
	}
	if _, err := db.Client.Create(ctx, "events", map[string]any{
		"id": 1_000_000, "value": 1, "revision": 0,
	}); err == nil {
		t.Fatal("published unique index accepted a duplicate value")
	}

	tables, err := db.Client.Tables(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(tables) != 1 || len(tables[0].Indexes) != 2 {
		t.Fatalf("final schema = %#v", tables)
	}
	var valueType string
	for _, column := range tables[0].Columns {
		if column.Name == "value" {
			valueType = column.Type
		}
	}
	if valueType != "int64" {
		t.Fatalf("final value type = %q", valueType)
	}
	transitions, err := db.Client.SchemaTransitions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(transitions) != 3 {
		t.Fatalf("durable schema work = %#v, want three transitions", transitions)
	}
	for _, transition := range transitions {
		if transition.State != radclient.TransitionReady {
			t.Fatalf("transition did not finish: %#v", transition)
		}
	}
}

func applySchemaStageConcurrently(
	ctx context.Context,
	control *radclient.Client,
	url string,
	source string,
	actors int,
) (protocol.SchemaMigration, int, error) {
	type outcome struct {
		migration protocol.SchemaMigration
		err       error
	}
	start := make(chan struct{})
	outcomes := make(chan outcome, actors)
	var group sync.WaitGroup
	for range actors {
		client, err := radclient.Dial(url)
		if err != nil {
			return protocol.SchemaMigration{}, 0, err
		}
		group.Add(1)
		go func() {
			defer group.Done()
			<-start
			for {
				diff, err := client.SchemaDiff(ctx, source)
				if err != nil {
					outcomes <- outcome{err: err}
					return
				}
				if len(diff.Blocking) != 0 {
					outcomes <- outcome{err: fmt.Errorf("schema diff blocked: %#v", diff.Blocking)}
					return
				}
				migration, err := client.SchemaMigrate(ctx, source, protocol.SchemaIdentity{
					SchemaVersion: diff.CurrentVersion,
					SchemaHash:    diff.CurrentHash,
				}, false)
				if err == nil {
					outcomes <- outcome{migration: migration}
					return
				}
				if !radclient.IsConflict(err) {
					outcomes <- outcome{err: err}
					return
				}
				select {
				case <-ctx.Done():
					outcomes <- outcome{err: fmt.Errorf("retry schema apply: %w", ctx.Err())}
					return
				case <-time.After(time.Millisecond):
				}
			}
		}()
	}
	close(start)
	group.Wait()
	close(outcomes)

	var representative protocol.SchemaMigration
	var transitionIDs []string
	successes := 0
	for result := range outcomes {
		if result.err != nil {
			return protocol.SchemaMigration{}, successes, result.err
		}
		successes++
		if result.migration.DesiredHash == "" {
			return protocol.SchemaMigration{}, successes, errors.New("schema apply returned no desired hash")
		}
		if len(result.migration.TransitionIDs) > 0 {
			ids := slices.Clone(result.migration.TransitionIDs)
			slices.Sort(ids)
			if transitionIDs == nil {
				transitionIDs = ids
			} else if !slices.Equal(transitionIDs, ids) {
				return protocol.SchemaMigration{}, successes, fmt.Errorf(
					"idempotent schema applies returned different work: %v and %v",
					transitionIDs,
					ids,
				)
			}
			if len(result.migration.TransitionIDs) > len(representative.TransitionIDs) {
				representative = result.migration
			}
		} else if representative.DesiredHash == "" {
			representative = result.migration
		}
	}
	if representative.State == protocol.SchemaMigrationConverging {
		ready, err := control.WaitSchemaMigration(ctx, representative)
		return ready, successes, err
	}
	state, err := control.Schema(ctx)
	if err != nil {
		return protocol.SchemaMigration{}, successes, err
	}
	if state.SchemaHash != representative.DesiredHash {
		return protocol.SchemaMigration{}, successes, fmt.Errorf(
			"schema apply returned ready at %s, current schema is %s",
			representative.DesiredHash,
			state.SchemaHash,
		)
	}
	representative.SchemaState = state
	representative.State = protocol.SchemaMigrationReady
	return representative, successes, nil
}

func updateBlitzRevision(ctx context.Context, client *radclient.Client, id int, revision int64) error {
	var lastErr error
	for {
		_, found, err := client.Update(
			ctx,
			"events",
			map[string]any{"id": id},
			map[string]any{"revision": revision},
			nil,
		)
		if err == nil {
			if !found {
				return fmt.Errorf("created row %d disappeared before update", id)
			}
			return nil
		}
		lastErr = err
		select {
		case <-ctx.Done():
			return fmt.Errorf("update revision after retries: %v: %w", lastErr, ctx.Err())
		case <-time.After(time.Millisecond):
		}
	}
}

func recordWrite(stage *atomic.Int32, total *atomic.Uint64, during *[3]atomic.Uint64) {
	total.Add(1)
	if current := stage.Load(); current > 0 {
		during[current].Add(1)
	}
}

func pointReadBlitzRow(ctx context.Context, client *radclient.Client, id int64) error {
	record, found, err := client.Get(ctx, "events", map[string]any{"id": id})
	if err != nil {
		return err
	}
	if !found {
		return fmt.Errorf("stable row %d disappeared", id)
	}
	return validateBlitzRecord(record, false)
}

func scanBlitzRows(ctx context.Context, client *radclient.Client, minimum int) error {
	rows, err := client.Query(ctx, blitzScanQuery())
	if err != nil {
		return err
	}
	if len(rows) < minimum {
		return fmt.Errorf("snapshot contains %d rows, want at least %d", len(rows), minimum)
	}
	for i, row := range rows {
		if err := validateBlitzRecord(row, false); err != nil {
			return fmt.Errorf("row %d: %w", i, err)
		}
	}
	return nil
}

func blitzScanQuery() lirwire.Query {
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"e":       lirwire.Scan("events", "e"),
			"ordered": lirwire.Order("e", []lirwire.OrderTerm{{Expr: lirwire.Col("e", "id")}}),
		},
		Root: lirwire.Root{Node: "ordered", Cardinality: "many"},
	}
}

func validateBlitzRecord(record protocol.Record, replacementPublished bool) error {
	if _, ok := record["id"].(json.Number); !ok {
		return fmt.Errorf("id has type %T", record["id"])
	}
	if _, ok := record["revision"].(json.Number); !ok {
		return fmt.Errorf("revision has type %T", record["revision"])
	}
	switch record["value"].(type) {
	case json.Number:
	case string:
		if replacementPublished {
			return fmt.Errorf("value retained source representation: %#v", record)
		}
	default:
		return fmt.Errorf("value has type %T", record["value"])
	}
	return nil
}
