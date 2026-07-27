package exec

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"

	"github.com/Southclaws/rad/rad/engine/05_exec/mutate"
)

// CreateIndexWithBackfill registers a new index in the catalog and writes index
// entries for every existing row, in ONE transaction. If the backfill fails —
// most notably two rows sharing a value under a unique index — the
// registration rolls back with it, so the catalog never exposes an index
// whose entries don't exist. (A registered-but-empty index would let the
// planner silently drop rows: access-path choice must never change results.)
//
// The serializable transaction also closes the race with concurrent writers:
// the backfill's table scan tracks its range, so a row inserted while the
// backfill runs conflicts at commit rather than being missed.
func (e *Engine) CreateIndexWithBackfill(ctx context.Context, table string, def model.IndexDef) error {
	return e.CatalogTxn(ctx, func(tx *Tx, change *change.Mutation) error {
		return tx.CreateIndexWithBackfill(ctx, change, table, def)
	})
}

// CreateIndexWithBackfill registers and backfills an index inside an existing
// catalog transaction.
func (tx *Tx) CreateIndexWithBackfill(ctx context.Context, change *change.Mutation, table string, def model.IndexDef) error {
	tbl, idx, err := change.CreateIndex(ctx, table, def)
	if err != nil {
		return err
	}
	return mutate.BackfillIndex(ctx, tx.txn, tbl, idx)
}
