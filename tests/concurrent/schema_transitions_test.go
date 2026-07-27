package concurrent

import (
	"context"
	"encoding/json"
	"fmt"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestExternalColumnReplacementUnderChaoticTraffic(t *testing.T) {
	db := newChaosDB(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: 1, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	tableID, idColumn, valueColumn := pirwire.SchemaID(101), pirwire.SchemaID(1), pirwire.SchemaID(2)
	nullable := true
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.CreateTable(
		"create",
		pirwire.TableDefinition{
			ID: &tableID, Name: "evolving_values",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeInt64, Nullable: &nullable},
			},
			PrimaryKey: []string{"id"},
		},
	))); err != nil {
		t.Fatal(err)
	}
	const initialRows = 64
	if err := seedTransitionRows(ctx, db.Control, "evolving_values", lirwire.ScalarTypeInt64, initialRows); err != nil {
		t.Fatal(err)
	}

	stale := db.postgresClient(t)
	staleTx, err := stale.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := staleTx.Exec(ctx, `INSERT INTO evolving_values (id, value) VALUES (9000000, 9)`); err != nil {
		t.Fatal(err)
	}
	started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartColumnReplacement(
		"replace",
		tableID,
		valueColumn,
		pirwire.ColumnReplacementDefinition{Type: pirwire.ColumnTypeText, Nullable: true},
	)))
	if err != nil {
		t.Fatal(err)
	}
	if err := staleTx.Commit(ctx); !isPostgresConflict(err) {
		t.Fatalf("writer admitted before dual-write publication committed with %v, want SQLSTATE 40001", err)
	}
	transitionID := started.Statements[0].Control.TransitionID

	clients := make([]*radclient.Client, 6)
	for i := range clients {
		clients[i] = db.httpClient(t)
	}
	errs := make(chan error, len(clients)+3)
	var workers sync.WaitGroup
	workers.Add(1)
	go func() {
		defer workers.Done()
		if _, err := waitSchemaTransitionReady(ctx, db.Control, transitionID); err != nil {
			errs <- fmt.Errorf("automatic replacement worker: %w", err)
		}
	}()
	for actor, client := range clients {
		workers.Add(1)
		go func(actor int, client *radclient.Client) {
			defer workers.Done()
			for action := range 10 {
				id := int64(1_000_000 + actor*1_000 + action)
				if err := createAcrossReplacement(ctx, client, id); err != nil {
					errs <- fmt.Errorf("writer %d action %d: %w", actor, action, err)
					return
				}
			}
		}(actor, client)
	}
	for reader := range 2 {
		client := db.httpClient(t)
		workers.Add(1)
		go func(reader int) {
			defer workers.Done()
			for range 12 {
				_, err := client.Query(ctx, lirwire.Query{
					Nodes: map[string]lirwire.Node{
						"scan":  lirwire.Scan("evolving_values", "v"),
						"order": lirwire.Order("scan", []lirwire.OrderTerm{{Expr: lirwire.Col("v", "id")}}),
					},
					Root: lirwire.Root{Node: "order", Cardinality: "many"},
				})
				if err != nil && !radclient.IsConflict(err) {
					errs <- fmt.Errorf("reader %d: %w", reader, err)
					return
				}
				runtime.Gosched()
			}
		}(reader)
	}
	workers.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
	if t.Failed() {
		return
	}

	records, err := db.Control.Query(ctx, lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"scan":  lirwire.Scan("evolving_values", "v"),
			"order": lirwire.Order("scan", []lirwire.OrderTerm{{Expr: lirwire.Col("v", "id")}}),
		},
		Root: lirwire.Root{Node: "order", Cardinality: "many"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(records) != initialRows+len(clients)*10 {
		t.Fatalf("final row count = %d, want %d", len(records), initialRows+len(clients)*10)
	}
	for _, record := range records {
		if value := record["value"]; value != nil {
			if _, ok := value.(string); !ok {
				t.Fatalf("post-publication value has type %T: %#v", value, record)
			}
		}
	}
	transition, err := db.Control.SchemaTransition(ctx, transitionID)
	if err != nil || transition.State != radclient.TransitionReady {
		t.Fatalf("replacement terminal state = %+v err=%v", transition, err)
	}
}

func TestExternalNotNullValidationUnderChaoticTraffic(t *testing.T) {
	db := newChaosDB(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: 1, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	tableID, idColumn, valueColumn := pirwire.SchemaID(102), pirwire.SchemaID(1), pirwire.SchemaID(2)
	nullable := true
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.CreateTable(
		"create",
		pirwire.TableDefinition{
			ID: &tableID, Name: "validated_values",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText, Nullable: &nullable},
			},
			PrimaryKey: []string{"id"},
		},
	))); err != nil {
		t.Fatal(err)
	}
	if err := seedTransitionRows(ctx, db.Control, "validated_values", lirwire.ScalarTypeText, 64); err != nil {
		t.Fatal(err)
	}

	stale := db.postgresClient(t)
	staleTx, err := stale.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := staleTx.Exec(ctx, `INSERT INTO validated_values (id, value) VALUES (9000000, NULL)`); err != nil {
		t.Fatal(err)
	}
	started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartConstraintValidation(
		"validate",
		tableID,
		pirwire.ConstraintValidationDefinition{
			Name: "validated_values_value_required", Kind: "not_null", ColumnID: valueColumn,
		},
	)))
	if err != nil {
		t.Fatal(err)
	}
	if err := staleTx.Commit(ctx); !isPostgresConflict(err) {
		t.Fatalf("writer admitted before constraint enforcement committed with %v, want SQLSTATE 40001", err)
	}
	if _, err := stale.Exec(ctx, `INSERT INTO validated_values (id, value) VALUES (9000001, NULL)`); err == nil {
		t.Fatal("writer admitted after constraint enforcement inserted NULL")
	}
	transitionID := started.Statements[0].Control.TransitionID

	clients := make([]*radclient.Client, 5)
	for i := range clients {
		clients[i] = db.httpClient(t)
	}
	errs := make(chan error, len(clients)+1)
	var workers sync.WaitGroup
	workers.Add(1)
	go func() {
		defer workers.Done()
		if _, err := waitSchemaTransitionReady(ctx, db.Control, transitionID); err != nil {
			errs <- fmt.Errorf("automatic constraint worker: %w", err)
		}
	}()
	for actor, client := range clients {
		workers.Add(1)
		go func(actor int, client *radclient.Client) {
			defer workers.Done()
			for action := range 10 {
				id := int64(2_000_000 + actor*1_000 + action)
				if err := retryExternalWrite(ctx, func() error {
					_, err := client.Create(ctx, "validated_values", map[string]any{
						"id": id, "value": fmt.Sprintf("actor-%d-%d", actor, action),
					})
					return err
				}); err != nil {
					errs <- fmt.Errorf("writer %d action %d: %w", actor, action, err)
					return
				}
			}
		}(actor, client)
	}
	workers.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
	if t.Failed() {
		return
	}
	records, err := db.Control.Query(ctx, lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"scan":  lirwire.Scan("validated_values", "v"),
			"order": lirwire.Order("scan", []lirwire.OrderTerm{{Expr: lirwire.Col("v", "id")}}),
		},
		Root: lirwire.Root{Node: "order", Cardinality: "many"},
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, record := range records {
		if record["value"] == nil {
			t.Fatalf("valid constraint published with NULL row: %#v", record)
		}
	}
	table, ok, err := db.Catalog.GetTable(ctx, "validated_values")
	if err != nil || !ok {
		t.Fatalf("get validated table: ok=%v err=%v", ok, err)
	}
	column, ok := table.Column("value")
	if !ok || column.Nullable {
		t.Fatalf("published value column = %+v ok=%v", column, ok)
	}
}

func TestExternalCompatibleReplacementsUnderChaoticTraffic(t *testing.T) {
	db := newChaosDB(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: 1, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	tableID := pirwire.SchemaID(103)
	idColumn := pirwire.SchemaID(1)
	leftColumn := pirwire.SchemaID(2)
	rightColumn := pirwire.SchemaID(3)
	nullable := true
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.CreateTable(
		"create",
		pirwire.TableDefinition{
			ID: &tableID, Name: "composed_values",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &leftColumn, Name: "left_value", Type: pirwire.ColumnTypeInt64, Nullable: &nullable},
				{ID: &rightColumn, Name: "right_value", Type: pirwire.ColumnTypeInt64, Nullable: &nullable},
			},
			PrimaryKey: []string{"id"},
		},
	))); err != nil {
		t.Fatal(err)
	}
	for rowID := range 32 {
		if _, err := db.Control.Create(ctx, "composed_values", map[string]any{
			"id":          int64(rowID),
			"left_value":  int64(rowID * 2),
			"right_value": int64(rowID * 3),
		}); err != nil {
			t.Fatal(err)
		}
	}

	type startResult struct {
		id  string
		err error
	}
	started := make(chan startResult, 2)
	for _, spec := range []struct {
		name   string
		column pirwire.SchemaID
	}{
		{name: "replace_left", column: leftColumn},
		{name: "replace_right", column: rightColumn},
	} {
		spec := spec
		client := db.httpClient(t)
		go func() {
			id, err := startExternalReplacement(
				ctx,
				client,
				spec.name,
				tableID,
				spec.column,
			)
			started <- startResult{id: id, err: err}
		}()
	}
	transitionIDs := make([]string, 0, 2)
	for range 2 {
		result := <-started
		if result.err != nil {
			t.Fatal(result.err)
		}
		transitionIDs = append(transitionIDs, result.id)
	}

	errs := make(chan error, 16)
	var workers sync.WaitGroup
	for _, transitionID := range transitionIDs {
		transitionID := transitionID
		workers.Add(1)
		go func() {
			defer workers.Done()
			if _, err := waitSchemaTransitionReady(ctx, db.Control, transitionID); err != nil {
				errs <- fmt.Errorf("automatic replacement %s: %w", transitionID, err)
			}
		}()
	}
	for actor := range 6 {
		client := db.httpClient(t)
		workers.Add(1)
		go func(actor int, client *radclient.Client) {
			defer workers.Done()
			for action := range 10 {
				id := int64(3_000_000 + actor*1_000 + action)
				if err := createAcrossComposedReplacements(ctx, client, id); err != nil {
					errs <- fmt.Errorf("writer %d action %d: %w", actor, action, err)
					return
				}
				runtime.Gosched()
			}
		}(actor, client)
	}
	for reader := range 2 {
		client := db.httpClient(t)
		workers.Add(1)
		go func(reader int, client *radclient.Client) {
			defer workers.Done()
			for range 20 {
				if _, err := client.Query(ctx, lirwire.Query{
					Nodes: map[string]lirwire.Node{
						"scan": lirwire.Scan("composed_values", "v"),
						"order": lirwire.Order(
							"scan",
							[]lirwire.OrderTerm{{Expr: lirwire.Col("v", "id")}},
						),
					},
					Root: lirwire.Root{Node: "order", Cardinality: "many"},
				}); err != nil && !radclient.IsConflict(err) {
					errs <- fmt.Errorf("reader %d: %w", reader, err)
					return
				}
				runtime.Gosched()
			}
		}(reader, client)
	}
	workers.Wait()
	close(errs)
	for err := range errs {
		t.Error(err)
	}
	if t.Failed() {
		return
	}

	rows, err := db.Control.Query(ctx, lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"scan": lirwire.Scan("composed_values", "v"),
			"order": lirwire.Order(
				"scan",
				[]lirwire.OrderTerm{{Expr: lirwire.Col("v", "id")}},
			),
		},
		Root: lirwire.Root{Node: "order", Cardinality: "many"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 32+6*10 {
		t.Fatalf("composed final row count = %d, want %d", len(rows), 32+6*10)
	}
	for _, row := range rows {
		if row["left_value"] != nil {
			if _, ok := row["left_value"].(string); !ok {
				t.Fatalf("left replacement published mixed value: %#v", row)
			}
		}
		if row["right_value"] != nil {
			if _, ok := row["right_value"].(string); !ok {
				t.Fatalf("right replacement published mixed value: %#v", row)
			}
		}
	}
	for _, transitionID := range transitionIDs {
		transition, err := db.Control.SchemaTransition(ctx, transitionID)
		if err != nil || transition.State != radclient.TransitionReady {
			t.Fatalf("composed transition %s = %+v err=%v", transitionID, transition, err)
		}
	}
}

func startExternalReplacement(
	ctx context.Context,
	client *radclient.Client,
	name string,
	tableID pirwire.SchemaID,
	columnID pirwire.SchemaID,
) (string, error) {
	return retryAttempts(ctx, 100, 0, radclient.IsConflict, nil, func(int) (string, error) {
		result, err := client.Execute(ctx, pirwire.Prog(
			"",
			pirwire.StartColumnReplacement(
				name,
				tableID,
				columnID,
				pirwire.ColumnReplacementDefinition{
					Type: pirwire.ColumnTypeText, Nullable: true,
				},
			),
		))
		if err == nil {
			if len(result.Statements) != 1 || result.Statements[0].Control == nil {
				return "", fmt.Errorf("start %s returned no transition control", name)
			}
			return result.Statements[0].Control.TransitionID, nil
		}
		return "", err
	})
}

func createAcrossComposedReplacements(
	ctx context.Context,
	client *radclient.Client,
	id int64,
) error {
	shapes := [][2]any{
		{int64(id * 2), int64(id * 3)},
		{fmt.Sprintf("%d", id*2), int64(id * 3)},
		{int64(id * 2), fmt.Sprintf("%d", id*3)},
		{fmt.Sprintf("%d", id*2), fmt.Sprintf("%d", id*3)},
	}
	_, err := retryAttempts(ctx, 200, 0, replacementRaceRetryable, nil,
		func(attempt int) (struct{}, error) {
			shape := shapes[attempt%len(shapes)]
			_, err := client.Create(ctx, "composed_values", map[string]any{
				"id": id, "left_value": shape[0], "right_value": shape[1],
			})
			return struct{}{}, err
		})
	if err != nil {
		return fmt.Errorf("composed write %d: %w", id, err)
	}
	return nil
}

func createAcrossReplacement(
	ctx context.Context,
	client *radclient.Client,
	id int64,
) error {
	_, err := retryAttempts(ctx, 100, 0, replacementRaceRetryable, nil,
		func(attempt int) (struct{}, error) {
			value := any(id)
			if attempt%2 == 1 {
				value = fmt.Sprintf("%d", id)
			}
			_, err := client.Create(ctx, "evolving_values", map[string]any{"id": id, "value": value})
			return struct{}{}, err
		})
	if err != nil {
		return fmt.Errorf("write %d did not cross replacement: %w", id, err)
	}
	return nil
}

func retryExternalWrite(ctx context.Context, fn func() error) error {
	_, err := retryAttempts(ctx, 100, 0, func(err error) bool {
		return radclient.IsConflict(err) || strings.Contains(err.Error(), "finaliz")
	}, nil, func(int) (struct{}, error) {
		return struct{}{}, fn()
	})
	return err
}

func replacementRaceRetryable(err error) bool {
	return radclient.IsConflict(err) ||
		strings.Contains(err.Error(), "expects text") ||
		strings.Contains(err.Error(), "expects int64") ||
		strings.Contains(err.Error(), "finaliz")
}

func seedTransitionRows(
	ctx context.Context,
	client *radclient.Client,
	table string,
	valueType lirwire.ScalarType,
	count int,
) error {
	columns := []lirwire.RowsColumn{
		{Name: "id", Type: lirwire.ScalarTypeInt64},
		{Name: "value", Type: valueType},
	}
	rows := make([][]lirwire.Cell, count)
	for i := range count {
		id, err := lirwire.MakeCell(lirwire.ScalarTypeInt64, int64(i))
		if err != nil {
			return err
		}
		value := any(int64(i))
		if valueType == lirwire.ScalarTypeText {
			value = fmt.Sprintf("value-%d", i)
		}
		encoded, err := lirwire.MakeCell(valueType, value)
		if err != nil {
			return err
		}
		rows[i] = []lirwire.Cell{id, encoded}
	}
	relation := lirwire.Query{
		Nodes: map[string]lirwire.Node{"rows": lirwire.Rows("r", columns, rows)},
		Root:  lirwire.Root{Node: "rows", Cardinality: "many"},
	}
	raw, err := json.Marshal(relation)
	if err != nil {
		return err
	}
	_, err = client.Execute(ctx, pirwire.Prog("", pirwire.Create("seed", table, raw)))
	return err
}
