package rowstore

import (
	"bytes"
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

func emitInsertDeltas(ctx context.Context, view kv.KV, table model.Table, protocol model.WriteProtocol, row lir.Row, pk []byte) error {
	for _, sink := range protocol.DeltaSinks {
		columns := table.IndexColumnNames(sink.Index)
		tuple, err := codec.EncodeRowTuple(row, columns)
		if err != nil {
			return err
		}
		if sink.Index.Unique && !hasNull(row, columns) {
			if err := store.PutUniqueClaim(ctx, view, sink.TransitionID, tuple, pk); err != nil {
				return err
			}
		}
		if _, err := store.AppendIndexDelta(ctx, view, sink.TransitionID, sink.DeltaHardLimit, model.IndexDelta{
			Operation: model.IndexDeltaPut, PK: bytes.Clone(pk), Tuple: tuple,
		}); err != nil {
			return err
		}
	}
	return nil
}

func emitReplaceDeltas(ctx context.Context, view kv.KV, table model.Table, protocol model.WriteProtocol, before, after lir.Row, pk []byte) error {
	for _, sink := range protocol.DeltaSinks {
		columns := table.IndexColumnNames(sink.Index)
		oldTuple, err := codec.EncodeRowTuple(before, columns)
		if err != nil {
			return err
		}
		newTuple, err := codec.EncodeRowTuple(after, columns)
		if err != nil {
			return err
		}
		if bytes.Equal(oldTuple, newTuple) {
			continue
		}
		if sink.Index.Unique {
			if !hasNull(before, columns) {
				if err := store.DeleteUniqueClaim(ctx, view, sink.TransitionID, oldTuple, pk); err != nil {
					return err
				}
			}
			if !hasNull(after, columns) {
				if err := store.PutUniqueClaim(ctx, view, sink.TransitionID, newTuple, pk); err != nil {
					return err
				}
			}
		}
		if _, err := store.AppendIndexDelta(ctx, view, sink.TransitionID, sink.DeltaHardLimit, model.IndexDelta{
			Operation: model.IndexDeltaDelete, PK: bytes.Clone(pk), Tuple: oldTuple,
		}); err != nil {
			return err
		}
		if _, err := store.AppendIndexDelta(ctx, view, sink.TransitionID, sink.DeltaHardLimit, model.IndexDelta{
			Operation: model.IndexDeltaPut, PK: bytes.Clone(pk), Tuple: newTuple,
		}); err != nil {
			return err
		}
	}
	return nil
}

func emitDeleteDeltas(ctx context.Context, view kv.KV, table model.Table, protocol model.WriteProtocol, row lir.Row, pk []byte) error {
	for _, sink := range protocol.DeltaSinks {
		columns := table.IndexColumnNames(sink.Index)
		tuple, err := codec.EncodeRowTuple(row, columns)
		if err != nil {
			return err
		}
		if sink.Index.Unique && !hasNull(row, columns) {
			if err := store.DeleteUniqueClaim(ctx, view, sink.TransitionID, tuple, pk); err != nil {
				return err
			}
		}
		if _, err := store.AppendIndexDelta(ctx, view, sink.TransitionID, sink.DeltaHardLimit, model.IndexDelta{
			Operation: model.IndexDeltaDelete, PK: bytes.Clone(pk), Tuple: tuple,
		}); err != nil {
			return err
		}
	}
	return nil
}

func hasNull(row lir.Row, columns []string) bool {
	for _, column := range columns {
		if row[column].Null {
			return true
		}
	}
	return false
}
