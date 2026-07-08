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

	kv "rad/rad/01_kv"
	keyenc "rad/rad/01_kv/keyenc"
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	planner "rad/rad/04_planner"
)

// Engine executes reads and writes against tables in a single schema.
type Engine struct {
	store  kv.TransactionalKV
	cat    *catalog.Catalog
	schema string
}

func New(store kv.TransactionalKV, cat *catalog.Catalog, schema string) *Engine {
	return &Engine{store: store, cat: cat, schema: schema}
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
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	if err := fn(&Tx{e: e, txn: txn}); err != nil {
		return err
	}
	return txn.Commit(ctx)
}

// IsConflict reports whether err is a transaction conflict that can be
// resolved by retrying the whole Engine.Txn.
func IsConflict(err error) bool { return errors.Is(err, kv.ErrConflict) }

func (e *Engine) Catalog() *catalog.Catalog { return e.cat }

// Query plans and executes a logical query against the committed state.
func (e *Engine) Query(ctx context.Context, q lir.Query) ([]lir.Row, error) {
	return e.query(ctx, e.store, q)
}

// Query inside a transaction runs against its snapshot plus its own writes.
func (tx *Tx) Query(ctx context.Context, q lir.Query) ([]lir.Row, error) {
	return tx.e.query(ctx, tx.txn, q)
}

func (e *Engine) query(ctx context.Context, view kv.KV, q lir.Query) ([]lir.Row, error) {
	plan, err := planner.Plan(ctx, e.cat, e.schema, q)
	if err != nil {
		return nil, err
	}
	return Run(ctx, view, plan)
}

func (e *Engine) table(ctx context.Context, name string) (catalog.Table, error) {
	tbl, ok, err := e.cat.GetTable(ctx, e.schema, name)
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
	tbl, err := e.table(ctx, table)
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
// POC deviation from SQL: NULLs participate in uniqueness like ordinary
// values.
//
// Inside a serializable transaction the *requested* prefix range is tracked
// even when the scan returns nothing, so two concurrent inserts of the same
// unique value (different PKs, hence different index keys) conflict at
// commit instead of both succeeding.
func checkUniqueIndexes(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte) error {
	for _, idx := range tbl.Indexes {
		if !idx.Unique {
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

// GetByPrimaryKey fetches one row by its primary key values. key must contain
// exactly the primary key columns.
func (e *Engine) GetByPrimaryKey(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error) {
	return e.getByPrimaryKey(ctx, e.store, table, key)
}

func (tx *Tx) GetByPrimaryKey(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error) {
	return tx.e.getByPrimaryKey(ctx, tx.txn, table, key)
}

func (e *Engine) getByPrimaryKey(ctx context.Context, view kv.KV, table string, key lir.Row) (lir.Row, bool, error) {
	tbl, err := e.table(ctx, table)
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

// ScanTable streams every row of the table in primary key order.
func (e *Engine) ScanTable(ctx context.Context, table string) (RowIterator, error) {
	tbl, err := e.table(ctx, table)
	if err != nil {
		return nil, err
	}
	return scanTable(ctx, e.store, tbl)
}

// ScanTable inside a transaction sees the snapshot plus the transaction's
// own writes. The iterator must be closed before the transaction commits.
func (tx *Tx) ScanTable(ctx context.Context, table string) (RowIterator, error) {
	tbl, err := tx.e.table(ctx, table)
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

// ScanIndex returns the base rows whose indexed values match prefix. The
// prefix row must populate a leading subset of the index's columns (all
// columns for an exact match, fewer for a range of the leading columns).
func (e *Engine) ScanIndex(ctx context.Context, table string, index string, prefix lir.Row) ([]lir.Row, error) {
	tbl, idx, err := e.resolveIndex(ctx, table, index)
	if err != nil {
		return nil, err
	}
	return scanIndex(ctx, e.store, tbl, idx, prefix)
}

func (tx *Tx) ScanIndex(ctx context.Context, table string, index string, prefix lir.Row) ([]lir.Row, error) {
	tbl, idx, err := tx.e.resolveIndex(ctx, table, index)
	if err != nil {
		return nil, err
	}
	return scanIndex(ctx, tx.txn, tbl, idx, prefix)
}

func (e *Engine) resolveIndex(ctx context.Context, table, index string) (catalog.Table, catalog.Index, error) {
	tbl, err := e.table(ctx, table)
	if err != nil {
		return catalog.Table{}, catalog.Index{}, err
	}
	idx, ok := tbl.Index(index)
	if !ok {
		return catalog.Table{}, catalog.Index{}, fmt.Errorf("exec: table %q has no index %q", table, index)
	}
	return tbl, idx, nil
}

// scanIndex range-scans the index prefix, then fetches base rows by the
// primary key tuples stored in the index values.
func scanIndex(ctx context.Context, view kv.KV, tbl catalog.Table, idx catalog.Index, prefix lir.Row) ([]lir.Row, error) {
	var leading []string
	for _, name := range idx.Columns {
		if _, ok := prefix[name]; !ok {
			break
		}
		leading = append(leading, name)
	}
	if len(prefix) != len(leading) {
		return nil, fmt.Errorf("exec: index scan prefix must cover a leading subset of index %q columns %v", idx.Name, idx.Columns)
	}
	keyPrefix := IndexPrefix(tbl.ID, idx.ID)
	if len(leading) > 0 {
		tuple, err := encodeRowTuple(prefix, leading)
		if err != nil {
			return nil, err
		}
		keyPrefix = append(keyPrefix, tuple...)
	}

	it, err := view.Scan(ctx, keyPrefix, keyenc.PrefixEnd(keyPrefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()

	// Collect primary key tuples first, then fetch base rows.
	var pkTuples [][]byte
	for it.Next() {
		pkTuples = append(pkTuples, bytes.Clone(it.Value()))
	}
	if err := it.Err(); err != nil {
		return nil, err
	}

	rows := make([]lir.Row, 0, len(pkTuples))
	for _, pk := range pkTuples {
		raw, ok, err := view.Get(ctx, DataKey(tbl.ID, pk))
		if err != nil {
			return nil, err
		}
		if !ok {
			return nil, fmt.Errorf("exec: index %q points at missing row", idx.Name)
		}
		row, err := UnmarshalRow(tbl, raw)
		if err != nil {
			return nil, err
		}
		rows = append(rows, row)
	}
	return rows, nil
}
