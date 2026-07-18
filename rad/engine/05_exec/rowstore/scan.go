package rowstore

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

type Iterator interface {
	Next() (lir.Row, bool, error)
	Close() error
}

func Table(ctx context.Context, view kv.KV, name string) (model.Table, error) {
	table, ok, err := store.New(view).GetTable(ctx, name)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, fmt.Errorf("exec: table %q does not exist", name)
	}
	return table, nil
}

func Get(ctx context.Context, view kv.KV, table model.Table, key lir.Row) (lir.Row, []byte, bool, error) {
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
	row, err := codec.UnmarshalRow(table, raw)
	return row, pk, true, err
}

type tableIterator struct {
	it  kv.Iterator
	tbl model.Table
}

func (r *tableIterator) Next() (lir.Row, bool, error) {
	if !r.it.Next() {
		return nil, false, r.it.Err()
	}
	row, err := codec.UnmarshalRow(r.tbl, r.it.Value())
	return row, err == nil, err
}

func (r *tableIterator) Close() error { return r.it.Close() }

func ScanTable(ctx context.Context, view kv.KV, table model.Table) (Iterator, error) {
	prefix := codec.DataPrefix(table.ID)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	return &tableIterator{it: it, tbl: table}, nil
}
