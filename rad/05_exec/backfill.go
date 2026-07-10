package exec

import (
	"context"
	"fmt"

	kv "rad/rad/01_kv"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// AddIndexWithBackfill registers a new index in the catalog and writes index
// entries for every existing row, in ONE transaction. If the backfill fails —
// most notably two rows sharing a value under a unique index — the
// registration rolls back with it, so the catalog never exposes an index
// whose entries don't exist. (A registered-but-empty index would let the
// planner silently drop rows: access-path choice must never change results.)
//
// The serializable transaction also closes the race with concurrent writers:
// the backfill's table scan tracks its range, so a row inserted while the
// backfill runs conflicts at commit rather than being missed.
func (e *Engine) AddIndexWithBackfill(ctx context.Context, table string, def catalog.IndexDef) error {
	return e.Txn(ctx, func(tx *Tx) error {
		tbl, idx, err := catalog.AddIndexIn(ctx, tx.txn, table, def)
		if err != nil {
			return err
		}
		return backfillIndexIn(ctx, tx.txn, tbl, idx)
	})
}

// backfillIndexIn writes index entries for every existing row of tbl into
// idx against the caller's view. It takes resolved catalog values rather
// than names because the index being backfilled may not be committed yet —
// it is visible only inside the caller's transaction.
func backfillIndexIn(ctx context.Context, view kv.KV, tbl catalog.Table, idx catalog.Index) error {
	it, err := scanTable(ctx, view, tbl)
	if err != nil {
		return err
	}
	defer it.Close()

	seen := map[string]qir.Row{}
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
		if idx.Unique && !anyNullComponent(row, idx.Columns) {
			if prev, dup := seen[string(idxTuple)]; dup {
				return fmt.Errorf("exec: cannot backfill unique index %q: rows %v and %v share a value", idx.Name, prev, row)
			}
			seen[string(idxTuple)] = row
		}
		pkTuple, err := encodeRowTuple(row, tbl.PrimaryKey)
		if err != nil {
			return err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple), pkTuple); err != nil {
			return err
		}
	}
}
