package exec

import (
	"context"
	"fmt"

	lir "rad/rad/03_lir"
)

// BackfillIndex writes index entries for every existing row of table into
// the named (newly added) index, atomically. For unique indexes the
// backfill fails if two rows share an indexed tuple — the migration is
// rejected rather than recording a constraint the data violates.
func (e *Engine) BackfillIndex(ctx context.Context, table, index string) error {
	return e.Txn(ctx, func(tx *Tx) error {
		tbl, idx, err := e.resolveIndex(ctx, table, index)
		if err != nil {
			return err
		}

		it, err := scanTable(ctx, tx.txn, tbl)
		if err != nil {
			return err
		}
		defer it.Close()

		seen := map[string]lir.Row{}
		for {
			row, ok, err := it.Next()
			if err != nil {
				return err
			}
			if !ok {
				return nil
			}
			idxTuple, err := encodeRowTuple(row, idx.Columns)
			if err != nil {
				return err
			}
			if idx.Unique {
				if prev, dup := seen[string(idxTuple)]; dup {
					return fmt.Errorf("exec: cannot backfill unique index %q: rows %v and %v share a value", index, prev, row)
				}
				seen[string(idxTuple)] = row
			}
			pkTuple, err := encodeRowTuple(row, tbl.PrimaryKey)
			if err != nil {
				return err
			}
			if err := tx.txn.Put(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
				return err
			}
		}
	})
}
