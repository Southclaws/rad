package rowstore

import (
	"bytes"
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

// Iterator streams decoded rows from a physical table or index access path.
type Iterator interface {
	Next() (lir.Row, bool, error)
	Close() error
}

// Get reads one row using the complete bound table definition.
func Get(ctx context.Context, view kv.KV, table model.Table, key lir.Row) (lir.Row, []byte, bool, error) {
	if err := admitTable(ctx, view, table); err != nil {
		return nil, nil, false, err
	}
	return GetColumns(ctx, view, table, key, table.Columns)
}

// GetColumns fetches and decodes only columns already admitted through a bound
// plan's CatalogDependencies.
func GetColumns(ctx context.Context, view kv.KV, table model.Table, key lir.Row, columns []model.Column) (lir.Row, []byte, bool, error) {
	if len(key) != len(table.PrimaryKey) {
		return nil, nil, false, fmt.Errorf("exec: primary key for %q needs %d columns, got %d", table.Name, len(table.PrimaryKey), len(key))
	}
	for _, column := range table.PrimaryKey {
		if _, ok := key[column]; !ok {
			return nil, nil, false, fmt.Errorf("exec: primary key for %q missing column %q", table.Name, column)
		}
	}
	pk, err := codec.EncodeRowTuple(key, table.PrimaryKey)
	if err != nil {
		return nil, nil, false, err
	}
	raw, ok, err := view.Get(ctx, codec.DataKey(table.ID, pk))
	if err != nil || !ok {
		return nil, pk, ok, err
	}
	row, err := codec.UnmarshalRowColumns(table, columns, raw)
	return row, pk, true, err
}

type tableIterator struct {
	it      kv.Iterator
	tbl     model.Table
	columns []model.Column
}

func (r *tableIterator) Next() (lir.Row, bool, error) {
	if !r.it.Next() {
		return nil, false, r.it.Err()
	}
	row, err := codec.UnmarshalRowColumns(r.tbl, r.columns, r.it.Value())
	return row, err == nil, err
}

func (r *tableIterator) Close() error { return r.it.Close() }

// ScanTable scans rows using the complete bound table definition.
func ScanTable(ctx context.Context, view kv.KV, table model.Table) (Iterator, error) {
	if err := admitTable(ctx, view, table); err != nil {
		return nil, err
	}
	return ScanTableColumns(ctx, view, table, table.Columns)
}

// ScanTableColumns scans rows while decoding only columns already admitted
// through a bound plan's CatalogDependencies.
func ScanTableColumns(ctx context.Context, view kv.KV, table model.Table, columns []model.Column) (Iterator, error) {
	prefix := codec.DataPrefix(table.ID)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	return &tableIterator{it: it, tbl: table, columns: columns}, nil
}

// BatchRow is one decoded row plus its physical key and encoded primary key in
// a bounded schema-worker scan.
type BatchRow struct {
	Key []byte
	PK  []byte
	Row lir.Row
}

// RawBatchRow is one framed stored row plus its physical key and encoded
// primary key in a representation-transition scan.
type RawBatchRow struct {
	Key []byte
	PK  []byte
	Raw []byte
}

// ScanTableBatch returns at most limit rows strictly after cursor. Cursor is a
// complete physical data key returned by a previous call. The worker commits
// its index entries and returned next cursor atomically, so replay after a
// crash is harmless.
func ScanTableBatch(ctx context.Context, view kv.KV, table model.Table, cursor []byte, limit int) ([]BatchRow, []byte, error) {
	if err := admitTable(ctx, view, table); err != nil {
		return nil, nil, err
	}
	if limit <= 0 {
		limit = 128
	}
	prefix := codec.DataPrefix(table.ID)
	start := prefix
	if len(cursor) > 0 {
		start = append(bytes.Clone(cursor), 0)
	}
	it, err := view.Scan(ctx, start, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, nil, err
	}
	defer it.Close()
	rows := make([]BatchRow, 0, limit)
	var next []byte
	for len(rows) < limit && it.Next() {
		key := bytes.Clone(it.Key())
		if !bytes.HasPrefix(key, prefix) {
			return nil, nil, fmt.Errorf("exec: table scan for %q escaped its key prefix", table.Name)
		}
		row, err := codec.UnmarshalRow(table, it.Value())
		if err != nil {
			return nil, nil, err
		}
		rows = append(rows, BatchRow{Key: key, PK: bytes.Clone(key[len(prefix):]), Row: row})
		next = key
	}
	if err := it.Err(); err != nil {
		return nil, nil, err
	}
	return rows, next, nil
}

// ScanRawTableBatch is the representation-transition counterpart to
// ScanTableBatch. It returns framed row bytes so a worker can add a replacement
// physical cell without decoding or rewriting unrelated retired cells.
func ScanRawTableBatch(
	ctx context.Context,
	view kv.KV,
	table model.Table,
	cursor []byte,
	limit int,
) ([]RawBatchRow, []byte, error) {
	if err := admitTable(ctx, view, table); err != nil {
		return nil, nil, err
	}
	if limit <= 0 {
		limit = 128
	}
	prefix := codec.DataPrefix(table.ID)
	start := prefix
	if len(cursor) > 0 {
		start = append(bytes.Clone(cursor), 0)
	}
	it, err := view.Scan(ctx, start, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, nil, err
	}
	defer it.Close()
	rows := make([]RawBatchRow, 0, limit)
	var next []byte
	for len(rows) < limit && it.Next() {
		key := bytes.Clone(it.Key())
		if !bytes.HasPrefix(key, prefix) {
			return nil, nil, fmt.Errorf("exec: raw table scan for %q escaped its key prefix", table.Name)
		}
		rows = append(rows, RawBatchRow{
			Key: key,
			PK:  bytes.Clone(key[len(prefix):]),
			Raw: bytes.Clone(it.Value()),
		})
		next = key
	}
	if err := it.Err(); err != nil {
		return nil, nil, err
	}
	return rows, next, nil
}
