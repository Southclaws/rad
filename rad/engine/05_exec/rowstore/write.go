package rowstore

import (
	"bytes"
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func admitWriteProtocol(protocol model.WriteProtocol) error {
	if protocol.FinalizationGate == nil {
		return nil
	}
	gate := protocol.FinalizationGate
	return reject.Fail(reject.ReasonTransitionFinalizing,
		"catalog: table %q is briefly write-gated while transition %q finalizes %s %q",
		protocol.TableID, gate.TransitionID, gate.Kind, gate.ObjectID)
}

func Write(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte) error {
	if err := admitTable(ctx, view, tbl); err != nil {
		return err
	}
	protocol, err := store.ReadWriteProtocol(ctx, view, tbl)
	if err != nil {
		return err
	}
	if err := admitWriteProtocol(protocol); err != nil {
		return err
	}
	rowBytes, err := codec.MarshalRow(tbl, row)
	if err != nil {
		return err
	}
	if err := applyConstraintChecks(ctx, view, tbl, protocol, row, pkTuple); err != nil {
		return err
	}
	rowBytes, err = applyColumnReplacementWrites(ctx, view, tbl, protocol, row, pkTuple, rowBytes)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, codec.DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range protocol.ReadyIndexes {
		idxTuple, err := codec.EncodeRowTuple(row, tbl.IndexColumnNames(idx))
		if err != nil {
			return err
		}
		if err := view.Put(ctx, codec.IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
	return emitInsertDeltas(ctx, view, tbl, protocol, row, pkTuple)
}

func Replace(ctx context.Context, view kv.KV, tbl model.Table, current, stored lir.Row, pkTuple []byte) error {
	if err := admitTable(ctx, view, tbl); err != nil {
		return err
	}
	protocol, err := store.ReadWriteProtocol(ctx, view, tbl)
	if err != nil {
		return err
	}
	if err := admitWriteProtocol(protocol); err != nil {
		return err
	}
	rowBytes, err := codec.MarshalRow(tbl, stored)
	if err != nil {
		return err
	}
	if err := applyConstraintChecks(ctx, view, tbl, protocol, stored, pkTuple); err != nil {
		return err
	}
	rowBytes, err = applyColumnReplacementWrites(ctx, view, tbl, protocol, stored, pkTuple, rowBytes)
	if err != nil {
		return err
	}
	if err := view.Put(ctx, codec.DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return err
	}
	for _, idx := range protocol.ReadyIndexes {
		columns := tbl.IndexColumnNames(idx)
		oldTuple, err := codec.EncodeRowTuple(current, columns)
		if err != nil {
			return err
		}
		newTuple, err := codec.EncodeRowTuple(stored, columns)
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
	return emitReplaceDeltas(ctx, view, tbl, protocol, current, stored, pkTuple)
}

func Delete(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte) error {
	if err := admitTable(ctx, view, tbl); err != nil {
		return err
	}
	protocol, err := store.ReadWriteProtocol(ctx, view, tbl)
	if err != nil {
		return err
	}
	if err := admitWriteProtocol(protocol); err != nil {
		return err
	}
	for _, idx := range protocol.ReadyIndexes {
		idxTuple, err := codec.EncodeRowTuple(row, tbl.IndexColumnNames(idx))
		if err != nil {
			return err
		}
		if err := view.Delete(ctx, codec.IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple)); err != nil {
			return err
		}
	}
	if err := view.Delete(ctx, codec.DataKey(tbl.ID, pkTuple)); err != nil {
		return err
	}
	if err := clearColumnReplacementViolations(ctx, view, protocol, pkTuple); err != nil {
		return err
	}
	if err := clearConstraintViolations(ctx, view, protocol, pkTuple); err != nil {
		return err
	}
	return emitDeleteDeltas(ctx, view, tbl, protocol, row, pkTuple)
}
