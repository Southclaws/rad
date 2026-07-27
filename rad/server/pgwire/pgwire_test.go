package pgwire

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgconn"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

func TestPostgresTransactionsUseConnectionOwnedSlateTransactions(t *testing.T) {
	ctx := context.Background()
	addr := serveTestPostgres(t)
	admin := connectTestPostgres(t, addr)
	if got := admin.PgConn().ParameterStatus("default_transaction_isolation"); got != "serializable" {
		t.Fatalf("advertised default transaction isolation = %q, want serializable", got)
	}
	if _, err := admin.Exec(ctx, `CREATE TABLE tx_items (id bigint PRIMARY KEY, value varchar NOT NULL)`); err != nil {
		t.Fatal(err)
	}

	t.Run("isolation requests are honest", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.Serializable})
		if err != nil {
			t.Fatal(err)
		}
		if err := tx.Rollback(ctx); err != nil {
			t.Fatal(err)
		}
		_, err = conn.BeginTx(ctx, pgx.TxOptions{IsoLevel: pgx.ReadCommitted})
		if err == nil {
			t.Fatal("READ COMMITTED succeeded with serializable Slate semantics")
		}
	})

	t.Run("rollback and read your writes", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (1, 'pending')`); err != nil {
			t.Fatal(err)
		}
		if got := countItems(t, tx, 1); got != 1 {
			t.Fatalf("inside count = %d, want 1", got)
		}
		if got := countItems(t, admin, 1); got != 0 {
			t.Fatalf("uncommitted row leaked to other session: count = %d", got)
		}
		if err := tx.Rollback(ctx); err != nil {
			t.Fatal(err)
		}
		if got := countItems(t, admin, 1); got != 0 {
			t.Fatalf("rolled-back row visible: count = %d", got)
		}
	})

	t.Run("commit", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (2, 'committed')`); err != nil {
			t.Fatal(err)
		}
		if err := tx.Commit(ctx); err != nil {
			t.Fatal(err)
		}
		if got := countItems(t, admin, 2); got != 1 {
			t.Fatalf("committed row count = %d, want 1", got)
		}
	})

	t.Run("disconnect rolls back", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (3, 'abandoned')`); err != nil {
			t.Fatal(err)
		}
		if err := conn.Close(ctx); err != nil {
			t.Fatal(err)
		}
		if got := countItems(t, admin, 3); got != 0 {
			t.Fatalf("disconnected transaction committed: count = %d", got)
		}
	})

	t.Run("statement error aborts transaction", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (2, 'duplicate')`); err == nil {
			t.Fatal("duplicate insert succeeded")
		}
		_, err = tx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (4, 'must not run')`)
		var pgErr *pgconn.PgError
		if !errors.As(err, &pgErr) || pgErr.Code != "25P02" {
			t.Fatalf("error = %v, want SQLSTATE 25P02", err)
		}
		if err := tx.Commit(ctx); !errors.Is(err, pgx.ErrTxCommitRollback) {
			t.Fatalf("commit error = %v, want ErrTxCommitRollback", err)
		}
		if got := countItems(t, admin, 4); got != 0 {
			t.Fatalf("statement after failure ran: count = %d", got)
		}
	})

	t.Run("parse error aborts transaction and rollback recovers connection", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (7, 'discarded')`); err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `SELEC deliberately_broken`); err == nil {
			t.Fatal("invalid SQL succeeded")
		}
		_, err = tx.Exec(ctx, `SELECT count(*) FROM tx_items`)
		var pgErr *pgconn.PgError
		if !errors.As(err, &pgErr) || pgErr.Code != "25P02" {
			t.Fatalf("error after parse failure = %v, want SQLSTATE 25P02", err)
		}
		if err := tx.Rollback(ctx); err != nil {
			t.Fatal(err)
		}
		if got := countItems(t, conn, 7); got != 0 {
			t.Fatalf("row survived rollback after parse failure: count = %d", got)
		}
	})

	t.Run("transaction catalog is visible only to its session", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `CREATE TABLE tx_private (id bigint PRIMARY KEY)`); err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_private (id) VALUES (1)`); err != nil {
			t.Fatal(err)
		}
		var count int64
		if err := tx.QueryRow(ctx, `SELECT count(*) FROM tx_private`).Scan(&count); err != nil || count != 1 {
			t.Fatalf("transaction catalog query: count=%d err=%v", count, err)
		}
		if _, err := admin.Exec(ctx, `SELECT count(*) FROM tx_private`); err == nil {
			t.Fatal("other session saw uncommitted table")
		}
		if err := tx.Rollback(ctx); err != nil {
			t.Fatal(err)
		}
		if _, err := admin.Exec(ctx, `SELECT count(*) FROM tx_private`); err == nil {
			t.Fatal("rolled-back table remained visible")
		}
	})

	t.Run("transaction catalog commits atomically with data", func(t *testing.T) {
		conn := connectTestPostgres(t, addr)
		tx, err := conn.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `CREATE TABLE tx_committed (id bigint PRIMARY KEY)`); err != nil {
			t.Fatal(err)
		}
		if _, err := tx.Exec(ctx, `INSERT INTO tx_committed (id) VALUES (1)`); err != nil {
			t.Fatal(err)
		}
		if err := tx.Commit(ctx); err != nil {
			t.Fatal(err)
		}
		var count int64
		if err := admin.QueryRow(ctx, `SELECT count(*) FROM tx_committed`).Scan(&count); err != nil || count != 1 {
			t.Fatalf("committed transaction catalog query: count=%d err=%v", count, err)
		}
	})

	t.Run("commit conflict belongs to whole transaction", func(t *testing.T) {
		left := connectTestPostgres(t, addr)
		right := connectTestPostgres(t, addr)
		leftTx, err := left.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		rightTx, err := right.Begin(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := leftTx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (5, 'left')`); err != nil {
			t.Fatal(err)
		}
		if _, err := rightTx.Exec(ctx, `INSERT INTO tx_items (id, value) VALUES (5, 'right')`); err != nil {
			t.Fatal(err)
		}
		if err := leftTx.Commit(ctx); err != nil {
			t.Fatal(err)
		}
		err = rightTx.Commit(ctx)
		var pgErr *pgconn.PgError
		if !errors.As(err, &pgErr) || pgErr.Code != "40001" {
			t.Fatalf("commit error = %v, want SQLSTATE 40001", err)
		}
		if got := countItems(t, admin, 5); got != 1 {
			t.Fatalf("conflict committed wrong row count = %d", got)
		}
	})
}

type queryer interface {
	QueryRow(context.Context, string, ...any) pgx.Row
}

func countItems(t *testing.T, q queryer, id int64) int64 {
	t.Helper()
	var count int64
	if err := q.QueryRow(context.Background(), `SELECT count(*) FROM tx_items WHERE id = $1`, id).Scan(&count); err != nil {
		t.Fatal(err)
	}
	return count
}

func serveTestPostgres(t *testing.T) string {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	cat := catalog.New(store)
	mode, err := cat.InitMode(ctx, model.ModeDirect)
	if err != nil {
		t.Fatal(err)
	}
	db := frontend.Open(store)
	server, err := New(db, cat, mode, slog.New(slog.DiscardHandler))
	if err != nil {
		t.Fatal(err)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- server.Serve(listener) }()
	t.Cleanup(func() {
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := server.Shutdown(shutdownCtx); err != nil {
			t.Errorf("shutdown: %v", err)
		}
		select {
		case err := <-done:
			if err != nil {
				t.Errorf("serve: %v", err)
			}
		case <-time.After(5 * time.Second):
			t.Error("pgwire server did not stop")
		}
		if err := store.Close(); err != nil {
			t.Errorf("close store: %v", err)
		}
	})
	return listener.Addr().String()
}

func connectTestPostgres(t *testing.T, addr string) *pgx.Conn {
	t.Helper()
	conn, err := pgx.Connect(context.Background(), fmt.Sprintf("postgres://test:test@%s/rad?sslmode=disable", addr))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if !conn.IsClosed() {
			_ = conn.Close(context.Background())
		}
	})
	return conn
}
