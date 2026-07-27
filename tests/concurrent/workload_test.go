package concurrent

import (
	"context"
	"encoding/json"
	"fmt"
	"math/rand"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

const (
	itemsTableID       = pirwire.SchemaID(1)
	probeTableID       = pirwire.SchemaID(2)
	probePayloadColumn = pirwire.SchemaID(2)
	indexName          = "items_bucket_online"
)

type expectedRow struct {
	ID         int64
	Value      string
	Bucket     string
	Generation int64
}

type workload struct {
	scenario scenario
	db       *chaosDB
	journal  *journal
	director *director

	httpWriters []*radclient.Client
	httpReaders []*radclient.Client
	pgWriters   []*pgx.Conn
	pgReaders   []*pgx.Conn

	transitionID string
	lastProgress uint64
	expected     map[int64]expectedRow
}

func newWorkload(t *testing.T, s scenario) *workload {
	t.Helper()
	db := newChaosDB(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: s.IndexBatchSize, BatchesBeforeYield: 1, YieldInterval: 5 * time.Millisecond,
	}))
	j := newJournal(s)
	w := &workload{
		scenario: s,
		db:       db,
		journal:  j,
		director: &director{journal: j, random: rand.New(rand.NewSource(s.Seed))},
		expected: make(map[int64]expectedRow),
	}
	for range s.HTTPWriters {
		w.httpWriters = append(w.httpWriters, db.httpClient(t))
	}
	for range s.HTTPReaders {
		w.httpReaders = append(w.httpReaders, db.httpClient(t))
	}
	for range s.PostgresWriters {
		w.pgWriters = append(w.pgWriters, db.postgresClient(t))
	}
	for range s.PostgresReaders {
		w.pgReaders = append(w.pgReaders, db.postgresClient(t))
	}
	return w
}

func (w *workload) bootstrap(ctx context.Context) error {
	itemsID, probeID := itemsTableID, probeTableID
	idColumn, valueColumn := pirwire.SchemaID(1), pirwire.SchemaID(2)
	generationColumn, bucketColumn := pirwire.SchemaID(3), pirwire.SchemaID(4)
	probeIDColumn := pirwire.SchemaID(1)
	probePayloadID := probePayloadColumn
	if _, err := w.db.Control.Execute(ctx, pirwire.Prog("",
		pirwire.CreateTable("items", pirwire.TableDefinition{
			ID: &itemsID, Name: "items",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
				{ID: &generationColumn, Name: "generation", Type: pirwire.ColumnTypeInt64},
				{ID: &bucketColumn, Name: "bucket", Type: pirwire.ColumnTypeText},
			},
			PrimaryKey: []string{"id"},
		}),
		pirwire.CreateTable("probe", pirwire.TableDefinition{
			ID: &probeID, Name: "catalog_probe",
			Columns: []pirwire.ColumnDefinition{
				{ID: &probeIDColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &probePayloadID, Name: "payload", Type: pirwire.ColumnTypeText},
			},
			PrimaryKey: []string{"id"},
		}),
	)); err != nil {
		return fmt.Errorf("create schema through PIR: %w", err)
	}
	if _, err := w.db.Control.Create(ctx, "catalog_probe", map[string]any{"id": int64(1), "payload": "stable"}); err != nil {
		return fmt.Errorf("seed sparse-row catalog probe: %w", err)
	}

	rows := make([]expectedRow, 0, w.scenario.InitialRows)
	for i := 1; i <= w.scenario.InitialRows; i++ {
		row := makeExpectedRow(int64(i), 0)
		rows = append(rows, row)
		w.expected[row.ID] = row
	}
	relation, err := rowRelation(rows)
	if err != nil {
		return err
	}
	if _, err := w.db.Control.Execute(ctx, pirwire.Prog("", pirwire.Create("seed", "items", relation))); err != nil {
		return fmt.Errorf("seed rows through PIR: %w", err)
	}

	fenced := makeExpectedRow(8_000_000, 0)
	oldWriter, err := w.pgWriters[0].BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
	if err != nil {
		return fmt.Errorf("begin pre-capture PostgreSQL transaction: %w", err)
	}
	if _, err := oldWriter.Exec(ctx,
		`INSERT INTO items (id, value, generation, bucket) VALUES ($1, $2, $3, $4)`,
		fenced.ID, fenced.Value, fenced.Generation, fenced.Bucket,
	); err != nil {
		_ = oldWriter.Rollback(context.Background())
		return fmt.Errorf("write before online-index capture: %w", err)
	}

	started, err := w.db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartIndexBuild(
		"build", itemsTableID, pirwire.IndexDefinition{Name: indexName, Columns: []string{"bucket"}},
	)))
	if err != nil {
		_ = oldWriter.Rollback(context.Background())
		return fmt.Errorf("start online index through PIR: %w", err)
	}
	if err := oldWriter.Commit(ctx); !isPostgresConflict(err) {
		return fmt.Errorf("writer admitted before capture committed with %v, want SQLSTATE 40001", err)
	}
	if _, err := w.pgWriters[0].Exec(ctx,
		`INSERT INTO items (id, value, generation, bucket) VALUES ($1, $2, $3, $4)`,
		fenced.ID, fenced.Value, fenced.Generation, fenced.Bucket,
	); err != nil {
		return fmt.Errorf("retry fenced writer after capture: %w", err)
	}
	w.expected[fenced.ID] = fenced
	if len(started.Statements) != 1 || started.Statements[0].Control == nil {
		return fmt.Errorf("start online index returned no control state: %+v", started)
	}
	w.transitionID = started.Statements[0].Control.TransitionID
	return nil
}

func rowRelation(rows []expectedRow) (pirwire.Relation, error) {
	columns := []lirwire.RowsColumn{
		{Name: "id", Type: lirwire.ScalarTypeInt64},
		{Name: "value", Type: lirwire.ScalarTypeText},
		{Name: "generation", Type: lirwire.ScalarTypeInt64},
		{Name: "bucket", Type: lirwire.ScalarTypeText},
	}
	cells := make([][]lirwire.Cell, len(rows))
	for i, row := range rows {
		id, err := lirwire.MakeCell(lirwire.ScalarTypeInt64, row.ID)
		if err != nil {
			return nil, err
		}
		value, err := lirwire.MakeCell(lirwire.ScalarTypeText, row.Value)
		if err != nil {
			return nil, err
		}
		generation, err := lirwire.MakeCell(lirwire.ScalarTypeInt64, row.Generation)
		if err != nil {
			return nil, err
		}
		bucket, err := lirwire.MakeCell(lirwire.ScalarTypeText, row.Bucket)
		if err != nil {
			return nil, err
		}
		cells[i] = []lirwire.Cell{id, value, generation, bucket}
	}
	query := lirwire.Query{
		Nodes: map[string]lirwire.Node{"rows": lirwire.Rows("r", columns, cells)},
		Root:  lirwire.Root{Node: "rows", Cardinality: "many"},
	}
	raw, err := json.Marshal(query)
	return pirwire.Relation(raw), err
}

func rowValue(id, generation int64) string {
	return fmt.Sprintf("row-%d-g-%d", id, generation)
}

func makeExpectedRow(id, generation int64) expectedRow {
	return expectedRow{
		ID: id, Generation: generation, Value: rowValue(id, generation),
		Bucket: fmt.Sprintf("bucket-%d", (id+generation)%7),
	}
}

func (w *workload) actionsForRound(round int) []action {
	actions := make([]action, 0,
		len(w.httpWriters)+len(w.pgWriters)+len(w.httpReaders)+len(w.pgReaders)+w.scenario.MetadataAdds+4)
	for actor, client := range w.httpWriters {
		actions = append(actions, w.httpWriterAction(round, actor, client))
	}
	for actor, conn := range w.pgWriters {
		actions = append(actions, w.postgresWriterAction(round, actor, conn))
	}
	for actor, client := range w.httpReaders {
		actions = append(actions, w.httpReaderAction(round, actor, client))
	}
	for actor, conn := range w.pgReaders {
		actions = append(actions, w.postgresReaderAction(round, actor, conn))
	}
	for actor := range w.scenario.MetadataAdds {
		actions = append(actions, w.addColumnAction(round, actor))
	}
	actions = append(actions,
		w.renameTableAction(round),
		w.renameColumnAction(round),
		w.indexSchedulerAction(round),
		w.inspectTransitionAction(round),
	)
	return actions
}
