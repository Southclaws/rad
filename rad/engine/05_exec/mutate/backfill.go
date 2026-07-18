package mutate

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func BackfillIndex(ctx context.Context, view kv.KV, table model.Table, index model.Index) error {
	it, err := rowstore.ScanTable(ctx, view, table)
	if err != nil {
		return err
	}
	defer it.Close()
	seen := map[string]lir.Row{}
	for {
		row, ok, err := it.Next()
		if err != nil || !ok {
			return err
		}
		tuple, err := codec.EncodeRowTuple(row, index.Columns)
		if err != nil {
			return err
		}
		if index.Unique && !anyNullComponent(row, index.Columns) {
			if previous, duplicate := seen[string(tuple)]; duplicate {
				return reject.Inputf("exec: cannot backfill unique index %q: rows %v and %v share a value", index.Name, previous, row)
			}
			seen[string(tuple)] = row
		}
		pk, err := codec.EncodeRowTuple(row, table.PrimaryKey)
		if err != nil {
			return err
		}
		if err := view.Put(ctx, codec.IndexKey(table.ID, index.ID, tuple, pk), pk); err != nil {
			return err
		}
	}
}
