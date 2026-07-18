package exec

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/mutate"
)

// Update merges set into the row identified by key (exactly the primary key
// columns) and rewrites affected index entries. Primary key columns cannot
// be changed. It reports whether the row existed.
func (e *Engine) Update(ctx context.Context, table string, key, set lir.Row) (lir.Row, bool, error) {
	var stored lir.Row
	var found bool
	err := e.Txn(ctx, func(tx *Tx) error {
		var err error
		stored, found, err = e.update(ctx, tx.txn, table, key, set)
		return err
	})
	if err != nil {
		return nil, false, err
	}
	return stored, found, nil
}

func (tx *Tx) Update(ctx context.Context, table string, key, set lir.Row) (lir.Row, bool, error) {
	return tx.e.update(ctx, tx.txn, table, key, set)
}

func (e *Engine) update(ctx context.Context, view kv.KV, table string, key, set lir.Row) (lir.Row, bool, error) {
	tbl, err := tableIn(ctx, view, table)
	if err != nil {
		return nil, false, err
	}
	return mutate.UpdateOne(ctx, view, tbl, key, set)
}

// Delete removes the row identified by key and reports whether it existed.
// References from other rows prevent deletion.
func (e *Engine) Delete(ctx context.Context, table string, key lir.Row) (bool, error) {
	var found bool
	err := e.Txn(ctx, func(tx *Tx) error {
		var err error
		found, err = e.delete(ctx, tx.txn, table, key)
		return err
	})
	return found, err
}

func (tx *Tx) Delete(ctx context.Context, table string, key lir.Row) (bool, error) {
	return tx.e.delete(ctx, tx.txn, table, key)
}

func (e *Engine) delete(ctx context.Context, view kv.KV, table string, key lir.Row) (bool, error) {
	tbl, err := tableIn(ctx, view, table)
	if err != nil {
		return false, err
	}
	return mutate.DeleteOne(ctx, view, tbl, key)
}
