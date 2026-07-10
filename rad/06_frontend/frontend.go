// Package frontend is Rad's public interface. Callers hand it relation-graph
// queries (03_qir) and rows; it drives the binder, planner, and executor and
// shapes results. This is where future input codecs (SQL, GraphQL, ORM
// bindings) lower into the IR — nothing above this package needs to know how
// queries are planned or executed, and nothing below it is public API.
package frontend

import (
	"context"

	kv "rad/rad/01_kv"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	exec "rad/rad/05_exec"
)

// DB is a handle to a Rad database. A Rad instance is exactly one database
// — no schema or database hierarchy exists.
type DB struct {
	cat *catalog.Catalog
	eng *exec.Engine
}

// Open attaches to the database on a store.
func Open(store kv.TransactionalKV) *DB {
	cat := catalog.New(store)
	return &DB{
		cat: cat,
		eng: exec.New(store, cat),
	}
}

// Catalog exposes schema metadata (table definitions, DDL).
func (db *DB) Catalog() *catalog.Catalog { return db.cat }

// CreateTable defines a new table.
func (db *DB) CreateTable(ctx context.Context, def catalog.TableDef) (catalog.Table, error) {
	return db.cat.CreateTable(ctx, def)
}

// Insert adds one row atomically (the row and its index entries commit
// together).
func (db *DB) Insert(ctx context.Context, table string, row qir.Row) error {
	return db.eng.Insert(ctx, table, row)
}

// Get fetches one row by primary key. key must contain exactly the primary
// key columns.
func (db *DB) Get(ctx context.Context, table string, key qir.Row) (qir.Row, bool, error) {
	return db.eng.GetByPrimaryKey(ctx, table, key)
}

// Create is Insert returning the stored row: the caller's values plus
// applied defaults (generated IDs, timestamps).
func (db *DB) Create(ctx context.Context, table string, row qir.Row) (qir.Row, error) {
	return db.eng.Create(ctx, table, row)
}

// Update merges set into the row identified by key, maintaining indexes and
// re-checking constraints. It returns the stored row and whether the row
// existed. Primary key columns cannot be changed.
func (db *DB) Update(ctx context.Context, table string, key, set qir.Row) (qir.Row, bool, error) {
	return db.eng.Update(ctx, table, key, set)
}

// Delete removes the row identified by key and its index entries. Deletes
// are restricted: they fail while other rows reference the row through a
// foreign key. It reports whether the row existed.
func (db *DB) Delete(ctx context.Context, table string, key qir.Row) (bool, error) {
	return db.eng.Delete(ctx, table, key)
}

// Tx is a transaction-scoped view of the database: snapshot reads plus the
// transaction's own buffered writes.
type Tx struct {
	tx *exec.Tx
}

func (tx *Tx) Insert(ctx context.Context, table string, row qir.Row) error {
	return tx.tx.Insert(ctx, table, row)
}

func (tx *Tx) Get(ctx context.Context, table string, key qir.Row) (qir.Row, bool, error) {
	return tx.tx.GetByPrimaryKey(ctx, table, key)
}

func (tx *Tx) Create(ctx context.Context, table string, row qir.Row) (qir.Row, error) {
	return tx.tx.Create(ctx, table, row)
}

func (tx *Tx) Update(ctx context.Context, table string, key, set qir.Row) (qir.Row, bool, error) {
	return tx.tx.Update(ctx, table, key, set)
}

func (tx *Tx) Delete(ctx context.Context, table string, key qir.Row) (bool, error) {
	return tx.tx.Delete(ctx, table, key)
}

// Txn runs fn inside a serializable transaction and commits it if fn
// returns nil. A commit-time concurrency race returns an error for which
// IsConflict reports true; retry the whole Txn (fn must be safe to re-run).
func (db *DB) Txn(ctx context.Context, fn func(tx *Tx) error) error {
	return db.eng.Txn(ctx, func(etx *exec.Tx) error {
		return fn(&Tx{tx: etx})
	})
}

// Begin starts an explicit transaction for callers that cannot use the Txn
// callback (e.g. servers holding a transaction across requests). The caller
// must Commit or Rollback.
func (db *DB) Begin(ctx context.Context) (*Tx, error) {
	etx, err := db.eng.Begin(ctx)
	if err != nil {
		return nil, err
	}
	return &Tx{tx: etx}, nil
}

// Commit atomically applies the transaction's writes.
func (tx *Tx) Commit(ctx context.Context) error { return tx.tx.Commit(ctx) }

// Rollback discards the transaction; safe after Commit.
func (tx *Tx) Rollback() error { return tx.tx.Rollback() }

// IsConflict reports whether err is a transaction conflict that can be
// resolved by retrying the whole Txn.
func IsConflict(err error) bool { return exec.IsConflict(err) }
