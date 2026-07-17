package exec

import (
	"context"
	"maps"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
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
	current, pkTuple, err := loadByPK(ctx, view, tbl, key)
	if err != nil || current == nil {
		return nil, false, err
	}

	for name := range set {
		if _, ok := tbl.Column(name); !ok {
			return nil, false, reject.Inputf("exec: table %q has no column %q", table, name)
		}
		if slices.Contains(tbl.PrimaryKey, name) {
			return nil, false, reject.Inputf("exec: cannot update primary key column %q", name)
		}
	}

	merged := maps.Clone(current)
	maps.Copy(merged, set)
	stored, err := normalizeRow(tbl, merged)
	if err != nil {
		return nil, false, err
	}

	if err := checkForeignKeysFor(ctx, view, tbl, stored, set); err != nil {
		return nil, false, err
	}
	if err := checkUniqueIndexesFor(ctx, view, tbl, stored, pkTuple, set); err != nil {
		return nil, false, err
	}

	if err := replaceRow(ctx, view, tbl, current, stored, pkTuple); err != nil {
		return nil, false, err
	}
	return stored, true, nil
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
	current, pkTuple, err := loadByPK(ctx, view, tbl, key)
	if err != nil || current == nil {
		return false, err
	}

	if err := checkNoReferences(ctx, view, tbl, current); err != nil {
		return false, err
	}
	if err := deleteRow(ctx, view, tbl, current, pkTuple); err != nil {
		return false, err
	}
	return true, nil
}

// loadByPK fetches a row and its encoded primary key tuple. A nil row means
// not found.
func loadByPK(ctx context.Context, view kv.KV, tbl catalog.Table, key lir.Row) (lir.Row, []byte, error) {
	if len(key) != len(tbl.PrimaryKey) {
		return nil, nil, reject.Inputf("exec: primary key of %q has %d columns, got %d values", tbl.Name, len(tbl.PrimaryKey), len(key))
	}
	pkTuple, err := encodeRowTuple(key, tbl.PrimaryKey)
	if err != nil {
		return nil, nil, err
	}
	raw, ok, err := view.Get(ctx, DataKey(tbl.ID, pkTuple))
	if err != nil || !ok {
		return nil, nil, err
	}
	row, err := UnmarshalRow(tbl, raw)
	if err != nil {
		return nil, nil, err
	}
	return row, pkTuple, nil
}
