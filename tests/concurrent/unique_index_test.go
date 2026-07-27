package concurrent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"runtime"
	"sync"
	"testing"
	"time"

	"github.com/jackc/pgx/v5/pgconn"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestOnlineUniqueIndexUnderChaoticOutsideInWrites(t *testing.T) {
	db := newChaosDB(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: 4, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	tableID := pirwire.SchemaID(60_000)
	idColumn := pirwire.SchemaID(1)
	tokenColumn := pirwire.SchemaID(2)
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("create", pirwire.TableDefinition{
		ID: &tableID, Name: "unique_items",
		Columns: []pirwire.ColumnDefinition{
			{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
			{ID: &tokenColumn, Name: "token", Type: pirwire.ColumnTypeText},
		},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}
	seed, err := uniqueSeedRelation(40)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.Create("seed", "unique_items", seed))); err != nil {
		t.Fatal(err)
	}
	unique := true
	started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartIndexBuild(
		"start", tableID, pirwire.IndexDefinition{
			Name: "unique_items_token_online", Columns: []string{"token"}, Unique: &unique,
		},
	)))
	if err != nil {
		t.Fatal(err)
	}
	if len(started.Statements) != 1 || started.Statements[0].Control == nil {
		t.Fatalf("start returned no transition: %+v", started)
	}
	transitionID := started.Statements[0].Control.TransitionID

	errCh := make(chan error, 6)
	var writers sync.WaitGroup
	for worker := range 6 {
		client := db.httpClient(t)
		writers.Add(1)
		go func(worker int, client *radclient.Client) {
			defer writers.Done()
			for action := range 6 {
				stableToken := fmt.Sprintf("worker-%d-revision-%03d", worker, action)
				if err := retryHTTPConflict(ctx, func() error {
					_, found, err := client.Update(ctx, "unique_items", map[string]any{"id": int64(worker + 1)},
						map[string]any{"token": stableToken}, nil)
					if err == nil && !found {
						return errors.New("stable writer row disappeared")
					}
					return err
				}); err != nil {
					errCh <- fmt.Errorf("worker %d update %d: %w", worker, action, err)
					return
				}
				id := int64(1_000_000 + worker*1_000 + action)
				if err := retryHTTPConflict(ctx, func() error {
					_, err := client.Create(ctx, "unique_items", map[string]any{
						"id": id, "token": fmt.Sprintf("ephemeral-%d-%03d", worker, action),
					})
					return err
				}); err != nil {
					errCh <- fmt.Errorf("worker %d create %d: %w", worker, action, err)
					return
				}
				runtime.Gosched()
			}
		}(worker, client)
	}
	writers.Wait()
	close(errCh)
	for err := range errCh {
		t.Error(err)
	}
	if t.Failed() {
		return
	}

	for {
		transition, err := db.Control.SchemaTransition(ctx, transitionID)
		if err != nil {
			t.Fatal(err)
		}
		if transition.State == radclient.TransitionFailed {
			t.Fatalf("unique build failed under unique traffic: %s", transition.LastError)
		}
		if transition.State == radclient.TransitionReady {
			break
		}
		select {
		case <-ctx.Done():
			t.Fatalf("%v; last transition state: %+v", ctx.Err(), transition)
		case <-time.After(time.Millisecond):
		}
	}
	rows, err := db.Auditor.ScanIndex(ctx, "unique_items", "unique_items_token_online", nil)
	if err != nil {
		t.Fatal(err)
	}
	seen := make(map[string]int64, len(rows))
	for _, row := range rows {
		token := row["token"].Text
		if previous, duplicate := seen[token]; duplicate {
			t.Fatalf("ready unique index contains duplicate token %q for %d and %d", token, previous, row["id"].Int64)
		}
		seen[token] = row["id"].Int64
	}
	if len(rows) != 76 {
		t.Fatalf("ready unique index has %d rows, want 76", len(rows))
	}

	pg := db.postgresClient(t)
	_, err = pg.Exec(ctx, `INSERT INTO unique_items (id, token) VALUES ($1, $2)`, int64(9_000_000), "seed-035")
	var pgErr *pgconn.PgError
	if !errors.As(err, &pgErr) || pgErr.Code != "23505" {
		t.Fatalf("duplicate through PostgreSQL = %v, want SQLSTATE 23505", err)
	}
}

func uniqueSeedRelation(count int) (pirwire.Relation, error) {
	columns := []lirwire.RowsColumn{
		{Name: "id", Type: lirwire.ScalarTypeInt64},
		{Name: "token", Type: lirwire.ScalarTypeText},
	}
	rows := make([][]lirwire.Cell, count)
	for i := 1; i <= count; i++ {
		id, err := lirwire.MakeCell(lirwire.ScalarTypeInt64, int64(i))
		if err != nil {
			return nil, err
		}
		token, err := lirwire.MakeCell(lirwire.ScalarTypeText, fmt.Sprintf("seed-%03d", i))
		if err != nil {
			return nil, err
		}
		rows[i-1] = []lirwire.Cell{id, token}
	}
	query := lirwire.Query{
		Nodes: map[string]lirwire.Node{"rows": lirwire.Rows("seed", columns, rows)},
		Root:  lirwire.Root{Node: "rows", Cardinality: "many"},
	}
	raw, err := json.Marshal(query)
	return pirwire.Relation(raw), err
}

func retryHTTPConflict(ctx context.Context, fn func() error) error {
	_, err := retryAttempts(ctx, 10_000, time.Microsecond, radclient.IsConflict, nil,
		func(int) (struct{}, error) {
			return struct{}{}, fn()
		})
	return err
}
