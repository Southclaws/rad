package rowstore

import (
	"bytes"
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

func Write(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte) error {
	rowBytes, err := codec.MarshalRow(tbl, row)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, codec.DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range tbl.Indexes {
		idxTuple, err := codec.EncodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		if err := view.Put(ctx, codec.IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return nil
}

func Replace(ctx context.Context, view kv.KV, tbl model.Table, current, stored lir.Row, pkTuple []byte) error {
	rowBytes, err := codec.MarshalRow(tbl, stored)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, codec.DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range tbl.Indexes {
		oldTuple, err := codec.EncodeRowTuple(current, idx.Columns)
		if err != nil {
			return err
		}
		newTuple, err := codec.EncodeRowTuple(stored, idx.Columns)
		if err != nil {
			return err
		}
		if bytes.Equal(oldTuple, newTuple) {
			continue
		}
		if err := view.Delete(ctx, codec.IndexKey(tbl.ID, idx.ID, oldTuple, pkTuple)); err != nil {
			return err
		}
		if err := view.Put(ctx, codec.IndexKey(tbl.ID, idx.ID, newTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return nil
}

func Delete(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte) error {
	for _, idx := range tbl.Indexes {
		idxTuple, err := codec.EncodeRowTuple(row, idx.Columns)
		if err != nil {
			return err
		}
		if err := view.Delete(ctx, codec.IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple)); err != nil {
			return err
		}
	}
	return view.Delete(ctx, codec.DataKey(tbl.ID, pkTuple))
}
