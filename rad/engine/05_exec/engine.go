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
	"bytes"
	"context"
	"errors"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Engine executes reads and writes against the database's tables.
type Engine struct {
	store kv.TransactionalKV
	cat   *catalog.Catalog
}

func New(store kv.TransactionalKV, cat *catalog.Catalog) *Engine {
	return &Engine{store: store, cat: cat}
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
func tableIn(ctx context.Context, view kv.KV, name string) (catalog.Table, error) {
	tbl, ok, err := catalog.NewReader(view).GetTable(ctx, name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, fmt.Errorf("exec: table %q does not exist", name)
	}
	return tbl, nil
}

// normalizeRow validates row against the table definition and returns a copy
// with every column present (absent nullable columns become explicit NULLs).
func normalizeRow(tbl catalog.Table, row lir.Row) (lir.Row, error) {
	for name := range row {
		if _, ok := tbl.Column(name); !ok {
			return nil, fmt.Errorf("exec: table %q has no column %q", tbl.Name, name)
		}
	}
	out := make(lir.Row, len(tbl.Columns))
	for _, col := range tbl.Columns {
		v, ok := row[col.Name]
		if !ok || v.Null {
			if !col.Nullable {
				return nil, fmt.Errorf("exec: column %q is not nullable", col.Name)
			}
			out[col.Name] = lir.Null(col.Type)
			continue
		}
		if v.Type != col.Type {
			return nil, fmt.Errorf("exec: column %q expects %s, got %s", col.Name, col.Type, v.Type)
		}
		out[col.Name] = v
	}
	return out, nil
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
	withDefaults, err := applyDefaults(tbl, row)
	if err != nil {
		return nil, err
	}
	stored, err := normalizeRow(tbl, withDefaults)
	if err != nil {
		return nil, err
	}

	pkTuple, err := encodeRowTuple(stored, tbl.PrimaryKey)
	if err != nil {
		return nil, err
	}
	rowKey := DataKey(tbl.ID, pkTuple)
	if _, ok, err := view.Get(ctx, rowKey); err != nil {
		return nil, err
	} else if ok {
		return nil, fmt.Errorf("exec: duplicate primary key in table %q", table)
	}

	if err := checkForeignKeys(ctx, view, tbl, stored); err != nil {
		return nil, err
	}
	if err := checkUniqueIndexes(ctx, view, tbl, stored, pkTuple); err != nil {
		return nil, err
	}

	rowBytes, err := MarshalRow(tbl, stored)
	if err != nil {
		return nil, err
	}
	if err := view.Put(ctx, rowKey, rowBytes); err != nil {
		return nil, err
	}
	for _, idx := range tbl.Indexes {
		idxTuple, err := encodeRowTuple(stored, idx.Columns)
		if err != nil {
			return nil, err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
			return nil, err
		}
	}
	return stored, nil
}

// checkForeignKeys verifies each referenced parent row exists. If any FK
// column is NULL the constraint is not checked (matches SQL semantics).
// Inside a serializable transaction the parent read is tracked, so a
// concurrent delete of the parent conflicts at commit.
func checkForeignKeys(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row) error {
	for _, fk := range tbl.ForeignKeys {
		vals := make([]lir.Value, len(fk.Columns))
		null := false
		for i, name := range fk.Columns {
			vals[i] = row[name]
			null = null || vals[i].Null
		}
		if null {
			continue
		}
		parentTuple, err := EncodeTuple(vals)
		if err != nil {
			return err
		}
		_, ok, err := view.Get(ctx, DataKey(fk.RefTableID, parentTuple))
		if err != nil {
			return err
		}
		if !ok {
			return fmt.Errorf("exec: foreign key %q violation: referenced row does not exist", fk.Name)
		}
	}
	return nil
}

// checkUniqueIndexes rejects the insert if a unique index already contains
// the indexed tuple under a different primary key. Because tuple encodings
// are self-delimiting and an index always has a fixed column count, a prefix
// scan on the full indexed tuple matches exactly the entries with equal
// indexed values.
//
// NULLs are distinct: a tuple with any NULL component is exempt from
// uniqueness (its index entry is still written). NULL equals nothing,
// including NULL.
//
// Inside a serializable transaction the *requested* prefix range is tracked
// even when the scan returns nothing, so two concurrent inserts of the same
// unique value (different PKs, hence different index keys) conflict at
// commit instead of both succeeding.
func checkUniqueIndexes(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte) error {
	for _, idx := range tbl.Indexes {
		if !idx.Unique || anyNullComponent(row, idx.Columns) {
			continue
		}
		idxTuple, err := encodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		prefix := append(IndexPrefix(tbl.ID, idx.ID), idxTuple...)
		it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
		if err != nil {
			return err
		}
		for it.Next() {
			if !bytes.Equal(it.Value(), pkTuple) {
				_ = it.Close()
				return fmt.Errorf("exec: unique index %q violation in table %q", idx.Name, tbl.Name)
			}
		}
		err = it.Err()
		if cerr := it.Close(); err == nil {
			err = cerr
		}
		if err != nil {
			return err
		}
	}
	return nil
}

// anyNullComponent reports whether any of the named columns is NULL in row.
func anyNullComponent(row lir.Row, cols []string) bool {
	for _, c := range cols {
		if row[c].Null {
			return true
		}
	}
	return false
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
	if len(key) != len(tbl.PrimaryKey) {
		return nil, false, fmt.Errorf("exec: primary key of %q has %d columns, got %d values", table, len(tbl.PrimaryKey), len(key))
	}
	pkTuple, err := encodeRowTuple(key, tbl.PrimaryKey)
	if err != nil {
		return nil, false, err
	}
	raw, ok, err := view.Get(ctx, DataKey(tbl.ID, pkTuple))
	if err != nil || !ok {
		return nil, false, err
	}
	row, err := UnmarshalRow(tbl, raw)
	if err != nil {
		return nil, false, err
	}
	return row, true, nil
}

// RowIterator streams rows from a scan.
type RowIterator interface {
	// Next returns the next row, or ok=false when the scan is exhausted.
	Next() (lir.Row, bool, error)
	Close() error
}

type kvRowIterator struct {
	it  kv.Iterator
	tbl catalog.Table
}

func (r *kvRowIterator) Next() (lir.Row, bool, error) {
	if !r.it.Next() {
		return nil, false, r.it.Err()
	}
	row, err := UnmarshalRow(r.tbl, r.it.Value())
	if err != nil {
		return nil, false, err
	}
	return row, true, nil
}

func (r *kvRowIterator) Close() error { return r.it.Close() }

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
	it, err := scanTable(ctx, txn, tbl)
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
	return scanTable(ctx, tx.txn, tbl)
}

func scanTable(ctx context.Context, view kv.KV, tbl catalog.Table) (RowIterator, error) {
	prefix := DataPrefix(tbl.ID)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	return &kvRowIterator{it: it, tbl: tbl}, nil
}
