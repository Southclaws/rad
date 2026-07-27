package exec

import (
	"context"

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
		stored, found, err = tx.Update(ctx, table, key, set)
		return err
	})
	if err != nil {
		return nil, false, err
	}
	return stored, found, nil
}

func (tx *Tx) Update(ctx context.Context, table string, key, set lir.Row) (lir.Row, bool, error) {
	tbl, err := tx.table(ctx, table)
	if err != nil {
		return nil, false, err
	}
	tx.e.yield(ctx, YieldBindingResolved, tbl.Name)
	stored, found, err := mutate.UpdateOne(ctx, tx.txn, tbl, key, set)
	if err == nil {
		tx.e.yield(ctx, YieldDependencyFencesAdmitted, tbl.Name)
	}
	return stored, found, err
}

// Delete removes the row identified by key and reports whether it existed.
// References from other rows prevent deletion.
func (e *Engine) Delete(ctx context.Context, table string, key lir.Row) (bool, error) {
	var found bool
	err := e.Txn(ctx, func(tx *Tx) error {
		var err error
		found, err = tx.Delete(ctx, table, key)
		return err
	})
	return found, err
}

func (tx *Tx) Delete(ctx context.Context, table string, key lir.Row) (bool, error) {
	tbl, err := tx.table(ctx, table)
	if err != nil {
		return false, err
	}
	tx.e.yield(ctx, YieldBindingResolved, tbl.Name)
	found, err := mutate.DeleteOne(ctx, tx.txn, tbl, key)
	if err == nil {
		tx.e.yield(ctx, YieldDependencyFencesAdmitted, tbl.Name)
	}
	return found, err
}
