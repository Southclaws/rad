// Package exec is the physical execution layer: it turns physical plans
// (04_planner) and write requests into KV operations — scans, gets, index
// lookups, joins, and constraint-checked writes.
//
// Every write runs inside a SerializableSnapshot transaction, so a row and
// its index entries commit atomically and constraint checks (duplicate PK,
// unique index, FK parent existence) are protected from concurrent writers
// by the KV's conflict detection. Multi-statement transactions are exposed
// via Engine.Txn.
package exec

import (
	"context"
	"errors"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/mutate"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Engine executes reads and writes against the database's tables.
type Engine struct {
	store kv.TransactionalKV
	cat   *catalog.Catalog
	recur RecursionLimits
}

// RecursionLimits bounds a recursive binding's fixpoint. They are execution
// safeguards, not query semantics — a terminating recursion never reaches
// them — so tune them to the deepest legitimate hierarchy and the most rows a
// query may return, not to any property of the language.
type RecursionLimits struct {
	MaxIterations int // frontier rounds before the query fails
	MaxRows       int // accumulated rows before the query fails
}

const (
	defaultMaxRecursionIterations = 10000
	defaultMaxRecursionRows       = 1_000_000
)

// Option configures an Engine at construction.
type Option func(*Engine)

// WithRecursionLimits overrides the fixpoint safeguards; a zero field keeps
// its default.
func WithRecursionLimits(l RecursionLimits) Option {
	return func(e *Engine) {
		if l.MaxIterations > 0 {
			e.recur.MaxIterations = l.MaxIterations
		}
		if l.MaxRows > 0 {
			e.recur.MaxRows = l.MaxRows
		}
	}
}

func New(store kv.TransactionalKV, cat *catalog.Catalog, opts ...Option) *Engine {
	e := &Engine{
		store: store,
		cat:   cat,
		recur: RecursionLimits{
			MaxIterations: defaultMaxRecursionIterations,
			MaxRows:       defaultMaxRecursionRows,
		},
	}
	for _, o := range opts {
		o(e)
	}
	return e
}

// Tx is a transaction-scoped view of the engine. All reads see the snapshot
// taken at Begin plus the transaction's own writes; all writes are buffered
// until the enclosing Engine.Txn commits.
type Tx struct {
	e   *Engine
	txn kv.Txn
}

// Txn runs fn inside a SerializableSnapshot transaction and commits it if fn
// returns nil. A commit-time concurrency race returns an error wrapping
// kv.ErrConflict; the caller can retry by calling Txn again (fn must be safe
// to re-run). If fn returns an error the transaction is rolled back.
func (e *Engine) Txn(ctx context.Context, fn func(tx *Tx) error) error {
	tx, err := e.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit(ctx)
}

// CatalogTxn runs a group of catalog operations and their associated data
// work as one serializable transaction and records one catalog revision when
// the group changes the schema.
func (e *Engine) CatalogTxn(ctx context.Context, fn func(tx *Tx, change *change.Mutation) error) error {
	return e.Txn(ctx, func(tx *Tx) error {
		_, err := change.Apply(ctx, tx.txn, func(change *change.Mutation) error {
			return fn(tx, change)
		})
		return err
	})
}

// Begin starts an explicit transaction. Prefer Txn where a callback fits;
// Begin exists for drivers (like the HTTP server) that hold a transaction
// open across requests. The caller must Commit or Rollback.
func (e *Engine) Begin(ctx context.Context) (*Tx, error) {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return nil, err
	}
	return &Tx{e: e, txn: txn}, nil
}

// Commit atomically applies the transaction's writes. The Tx is unusable
// afterwards.
func (tx *Tx) Commit(ctx context.Context) error { return tx.txn.Commit(ctx) }

// Rollback discards the transaction; safe to call after Commit.
func (tx *Tx) Rollback() error { return tx.txn.Rollback() }

// IsConflict reports whether err is a transaction conflict that can be
// resolved by retrying the whole Engine.Txn.
func IsConflict(err error) bool { return errors.Is(err, kv.ErrConflict) }

func (e *Engine) Catalog() *catalog.Catalog { return e.cat }

// tableIn resolves a table through the statement's KV view, so schema
// resolution shares the snapshot of the data reads and writes that follow —
// and, inside a serializable transaction, joins its read set.
func tableIn(ctx context.Context, view kv.KV, name string) (model.Table, error) {
	tbl, ok, err := store.New(view).GetTable(ctx, name)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("exec: table %q does not exist", name)
	}
	return tbl, nil
}

// Insert adds one row in its own transaction. For multi-row atomicity use
// Engine.Txn with Tx.Insert.
func (e *Engine) Insert(ctx context.Context, table string, row lir.Row) error {
	_, err := e.Create(ctx, table, row)
	return err
}

func (tx *Tx) Insert(ctx context.Context, table string, row lir.Row) error {
	_, err := tx.Create(ctx, table, row)
	return err
}

// Create is Insert returning the stored row — the caller's values plus
// applied defaults (generated IDs, timestamps).
func (e *Engine) Create(ctx context.Context, table string, row lir.Row) (lir.Row, error) {
	var stored lir.Row
	err := e.Txn(ctx, func(tx *Tx) error {
		var err error
		stored, err = tx.Create(ctx, table, row)
		return err
	})
	if err != nil {
		return nil, err
	}
	return stored, nil
}

func (tx *Tx) Create(ctx context.Context, table string, row lir.Row) (lir.Row, error) {
	return tx.e.insert(ctx, tx.txn, table, row)
}

func (e *Engine) insert(ctx context.Context, view kv.KV, table string, row lir.Row) (lir.Row, error) {
	tbl, err := tableIn(ctx, view, table)
	if err != nil {
		return nil, err
	}
	return mutate.CreateOne(ctx, view, tbl, row)
}

// GetByPrimaryKey fetches one row by its primary key values. key must contain
// exactly the primary key columns.
func (e *Engine) GetByPrimaryKey(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, false, err
	}
	defer txn.Rollback()
	return getByPrimaryKey(ctx, txn, table, key)
}

func (tx *Tx) GetByPrimaryKey(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error) {
	return getByPrimaryKey(ctx, tx.txn, table, key)
}

func getByPrimaryKey(ctx context.Context, view kv.KV, table string, key lir.Row) (lir.Row, bool, error) {
	tbl, err := tableIn(ctx, view, table)
	if err != nil {
		return nil, false, err
	}
	row, _, ok, err := rowstore.Get(ctx, view, tbl, key)
	return row, ok, err
}

// RowIterator streams rows from a scan.
type RowIterator interface {
	// Next returns the next row, or ok=false when the scan is exhausted.
	Next() (lir.Row, bool, error)
	Close() error
}

// ScanTable streams every row of the table in primary key order, from a
// snapshot held until the iterator is closed.
func (e *Engine) ScanTable(ctx context.Context, table string) (RowIterator, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, err
	}
	tbl, err := tableIn(ctx, txn, table)
	if err != nil {
		txn.Rollback()
		return nil, err
	}
	it, err := rowstore.ScanTable(ctx, txn, tbl)
	if err != nil {
		txn.Rollback()
		return nil, err
	}
	return &snapshotRowIterator{RowIterator: it, txn: txn}, nil
}

// snapshotRowIterator ties a statement-scoped snapshot's lifetime to the
// iterator it feeds.
type snapshotRowIterator struct {
	RowIterator
	txn kv.Txn
}

func (s *snapshotRowIterator) Close() error {
	err := s.RowIterator.Close()
	if rerr := s.txn.Rollback(); err == nil {
		err = rerr
	}
	return err
}

// ScanTable inside a transaction sees the snapshot plus the transaction's
// own writes. The iterator must be closed before the transaction commits.
func (tx *Tx) ScanTable(ctx context.Context, table string) (RowIterator, error) {
	tbl, err := tableIn(ctx, tx.txn, table)
	if err != nil {
		return nil, err
	}
	return rowstore.ScanTable(ctx, tx.txn, tbl)
}
