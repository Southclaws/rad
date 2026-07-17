package exec

import (
	"bytes"
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func prepareRow(tbl catalog.Table, row lir.Row) (lir.Row, error) {
	withDefaults, err := applyDefaults(tbl, row)
	if err != nil {
		return nil, err
	}
	return normalizeRow(tbl, withDefaults)
}

func writeRow(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte) error {
	rowBytes, err := MarshalRow(tbl, row)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range tbl.Indexes {
		idxTuple, err := encodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return nil
}

func replaceRow(ctx context.Context, view kv.KV, tbl catalog.Table, current, stored lir.Row, pkTuple []byte) error {
	rowBytes, err := MarshalRow(tbl, stored)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range tbl.Indexes {
		oldTuple, err := encodeRowTuple(current, idx.Columns)
		if err != nil {
			return err
		}
		newTuple, err := encodeRowTuple(stored, idx.Columns)
		if err != nil {
			return err
		}
		if bytes.Equal(oldTuple, newTuple) {
			continue
		}
		if err := view.Delete(ctx, IndexKey(tbl.ID, idx.ID, oldTuple, pkTuple)); err != nil {
			return err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, newTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return nil
}

func deleteRow(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte) error {
	for _, idx := range tbl.Indexes {
		idxTuple, err := encodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		if err := view.Delete(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple)); err != nil {
			return err
		}
	}
	return view.Delete(ctx, DataKey(tbl.ID, pkTuple))
}
