package concurrent

import (
	"context"
	"errors"
	"fmt"
	"runtime"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol"
)

type updateResult struct {
	record protocol.Record
	found  bool
}

func (w *workload) httpWriterAction(round, actor int, client *radclient.Client) action {
	stableID := int64(actor + 1)
	name := fmt.Sprintf("http-writer-%02d", actor)
	switch round % 3 {
	case 0:
		generation := int64(round + 1)
		row := makeExpectedRow(stableID, generation)
		w.expected[stableID] = row
		a := action{Round: round, Actor: name, Kind: "update", Detail: map[string]any{"id": stableID}}
		a.Run = func(ctx context.Context) (map[string]any, error) {
			result, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, radclient.IsConflict, func() (updateResult, error) {
				record, found, err := client.Update(ctx, "items", map[string]any{"id": stableID}, map[string]any{
					"value": row.Value, "generation": row.Generation, "bucket": row.Bucket,
				}, nil)
				return updateResult{record: record, found: found}, err
			})
			if err != nil {
				return nil, err
			}
			if !result.found {
				return nil, fmt.Errorf("writer-owned row %d disappeared", stableID)
			}
			return map[string]any{"id": stableID, "generation": generation}, validateRecord(result.record)
		}
		return a
	case 1:
		id := int64(100_000 + round*1000 + actor)
		row := makeExpectedRow(id, int64(round+1))
		w.expected[id] = row
		a := action{Round: round, Actor: name, Kind: "create", Detail: map[string]any{"id": id}}
		a.Run = func(ctx context.Context) (map[string]any, error) {
			record, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, radclient.IsConflict, func() (protocol.Record, error) {
				return client.Create(ctx, "items", map[string]any{
					"id": row.ID, "value": row.Value, "generation": row.Generation, "bucket": row.Bucket,
				})
			})
			if err != nil {
				return nil, err
			}
			return map[string]any{"id": id}, validateRecord(record)
		}
		return a
	default:
		id := int64(100_000 + (round-1)*1000 + actor)
		delete(w.expected, id)
		a := action{Round: round, Actor: name, Kind: "delete", Detail: map[string]any{"id": id}}
		a.Run = func(ctx context.Context) (map[string]any, error) {
			deleted, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, radclient.IsConflict, func() (bool, error) {
				return client.Delete(ctx, "items", map[string]any{"id": id})
			})
			if err != nil {
				return nil, err
			}
			if !deleted {
				return nil, fmt.Errorf("ephemeral row %d disappeared before delete", id)
			}
			return map[string]any{"id": id}, nil
		}
		return a
	}
}

func (w *workload) postgresWriterAction(round, actor int, conn *pgx.Conn) action {
	id := int64(1_000_000 + round*1000 + actor)
	row := makeExpectedRow(id, int64(round+1))
	w.expected[id] = row
	a := action{
		Round: round, Actor: fmt.Sprintf("pg-writer-%02d", actor), Kind: "transactional_create",
		Detail: map[string]any{"id": id},
	}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		_, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, isPostgresConflict, func() (struct{}, error) {
			tx, err := conn.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
			if err != nil {
				return struct{}{}, err
			}
			defer tx.Rollback(context.Background())
			var before int64
			if err := tx.QueryRow(ctx, `SELECT count(*) FROM items WHERE id = $1`, id).Scan(&before); err != nil {
				return struct{}{}, err
			}
			if before != 0 {
				return struct{}{}, fmt.Errorf("new id %d already existed", id)
			}
			if _, err := tx.Exec(ctx,
				`INSERT INTO items (id, value, generation, bucket) VALUES ($1, $2, $3, $4)`,
				row.ID, row.Value, row.Generation, row.Bucket,
			); err != nil {
				return struct{}{}, err
			}
			var gotValue string
			var gotBucket string
			var gotGeneration int64
			if err := tx.QueryRow(ctx, `SELECT value, generation, bucket FROM items WHERE id = $1`, id).
				Scan(&gotValue, &gotGeneration, &gotBucket); err != nil {
				return struct{}{}, err
			}
			if gotValue != row.Value || gotGeneration != row.Generation || gotBucket != row.Bucket {
				return struct{}{}, fmt.Errorf("read-own-write = (%q,%d,%q), want (%q,%d,%q)",
					gotValue, gotGeneration, gotBucket, row.Value, row.Generation, row.Bucket)
			}
			return struct{}{}, tx.Commit(ctx)
		})
		return map[string]any{"id": id}, err
	}
	return a
}

func (w *workload) httpReaderAction(round, actor int, client *radclient.Client) action {
	a := action{Round: round, Actor: fmt.Sprintf("http-reader-%02d", actor), Kind: "snapshot_scan"}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		records, err := client.Query(ctx, allItemsQuery())
		if err != nil {
			return nil, err
		}
		seen := make(map[int64]bool, len(records))
		for i, record := range records {
			if err := validateRecord(record); err != nil {
				return nil, fmt.Errorf("row %d: %w", i, err)
			}
			id, _ := int64Field(record, "id")
			if seen[id] {
				return nil, fmt.Errorf("snapshot returned duplicate id %d", id)
			}
			seen[id] = true
		}
		return map[string]any{"rows": len(records)}, nil
	}
	return a
}

func (w *workload) postgresReaderAction(round, actor int, conn *pgx.Conn) action {
	a := action{Round: round, Actor: fmt.Sprintf("pg-reader-%02d", actor), Kind: "repeatable_ilike_snapshot"}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		count, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, isPostgresConflict, func() (int64, error) {
			tx, err := conn.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
			if err != nil {
				return 0, err
			}
			defer tx.Rollback(context.Background())
			var first, folded, second int64
			if err := tx.QueryRow(ctx, `SELECT count(*) FROM items`).Scan(&first); err != nil {
				return 0, err
			}
			if err := tx.QueryRow(ctx, `SELECT count(*) FROM items WHERE value ILIKE '%G-%'`).Scan(&folded); err != nil {
				return 0, err
			}
			runtime.Gosched()
			if err := tx.QueryRow(ctx, `SELECT count(*) FROM items`).Scan(&second); err != nil {
				return 0, err
			}
			if first != second || folded != first {
				return 0, fmt.Errorf("one transaction observed counts total=%d folded=%d total_again=%d", first, folded, second)
			}
			if err := tx.Commit(ctx); err != nil {
				return 0, err
			}
			return first, nil
		})
		return map[string]any{"rows": count}, err
	}
	return a
}

func isPostgresConflict(err error) bool {
	var pgError *pgconn.PgError
	return errors.As(err, &pgError) && pgError.Code == "40001"
}
