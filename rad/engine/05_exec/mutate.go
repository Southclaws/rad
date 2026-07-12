package exec

import (
	"bytes"
	"context"
	"fmt"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
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

	// Validate the patch: known columns, immutable PK.
	for name := range set {
		if _, ok := tbl.Column(name); !ok {
			return nil, false, fmt.Errorf("exec: table %q has no column %q", table, name)
		}
		if slices.Contains(tbl.PrimaryKey, name) {
			return nil, false, fmt.Errorf("exec: cannot update primary key column %q", name)
		}
	}

	merged := make(lir.Row, len(current))
	for k, v := range current {
		merged[k] = v
	}
	for k, v := range set {
		merged[k] = v
	}
	stored, err := normalizeRow(tbl, merged)
	if err != nil {
		return nil, false, err
	}

	// Re-check constraints that the patch can affect.
	if err := checkForeignKeysFor(ctx, view, tbl, stored, set); err != nil {
		return nil, false, err
	}
	if err := checkUniqueIndexesFor(ctx, view, tbl, stored, pkTuple, set); err != nil {
		return nil, false, err
	}

	rowBytes, err := MarshalRow(tbl, stored)
	if err != nil {
		return nil, false, err
	}
	if err := view.Put(ctx, DataKey(tbl.ID, pkTuple), rowBytes); err != nil {
		return nil, false, err
	}

	// Rewrite index entries whose indexed values changed.
	for _, idx := range tbl.Indexes {
		if !touches(idx.Columns, set) {
			continue
		}
		oldTuple, err := encodeRowTuple(current, idx.Columns)
		if err != nil {
			return nil, false, err
		}
		newTuple, err := encodeRowTuple(stored, idx.Columns)
		if err != nil {
			return nil, false, err
		}
		if bytes.Equal(oldTuple, newTuple) {
			continue
		}
		if err := view.Delete(ctx, IndexKey(tbl.ID, idx.ID, oldTuple, pkTuple)); err != nil {
			return nil, false, err
		}
		if err := view.Put(ctx, IndexKey(tbl.ID, idx.ID, newTuple, pkTuple), pkTuple); err != nil {
			return nil, false, err
		}
	}
	return stored, true, nil
}

// Delete removes the row identified by key (exactly the primary key
// columns) along with its index entries. It fails if any other row
// references it through a foreign key (restrict semantics — the POC has no
// cascades). It reports whether the row existed.
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

	if err := e.checkNoReferences(ctx, view, tbl, current); err != nil {
		return false, err
	}

	for _, idx := range tbl.Indexes {
		idxTuple, err := encodeRowTuple(current, idx.Columns)
		if err != nil {
			return false, err
		}
		if err := view.Delete(ctx, IndexKey(tbl.ID, idx.ID, idxTuple, pkTuple)); err != nil {
			return false, err
		}
	}
	if err := view.Delete(ctx, DataKey(tbl.ID, pkTuple)); err != nil {
		return false, err
	}
	return true, nil
}

// loadByPK fetches a row and its encoded primary key tuple. A nil row means
// not found.
func loadByPK(ctx context.Context, view kv.KV, tbl catalog.Table, key lir.Row) (lir.Row, []byte, error) {
	if len(key) != len(tbl.PrimaryKey) {
		return nil, nil, fmt.Errorf("exec: primary key of %q has %d columns, got %d values", tbl.Name, len(tbl.PrimaryKey), len(key))
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

// touches reports whether the patch changes any of the named columns.
func touches(columns []string, set lir.Row) bool {
	for _, c := range columns {
		if _, ok := set[c]; ok {
			return true
		}
	}
	return false
}

// checkForeignKeysFor re-validates only the foreign keys whose columns the
// patch touches.
func checkForeignKeysFor(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, set lir.Row) error {
	scoped := tbl
	scoped.ForeignKeys = nil
	for _, fk := range tbl.ForeignKeys {
		if touches(fk.Columns, set) {
			scoped.ForeignKeys = append(scoped.ForeignKeys, fk)
		}
	}
	return checkForeignKeys(ctx, view, scoped, row)
}

// checkUniqueIndexesFor re-validates only the unique indexes whose columns
// the patch touches.
func checkUniqueIndexesFor(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row, pkTuple []byte, set lir.Row) error {
	scoped := tbl
	scoped.Indexes = nil
	for _, idx := range tbl.Indexes {
		if idx.Unique && touches(idx.Columns, set) {
			scoped.Indexes = append(scoped.Indexes, idx)
		}
	}
	return checkUniqueIndexes(ctx, view, scoped, row, pkTuple)
}

// checkNoReferences enforces delete-restrict: no row in any table may hold
// a foreign key pointing at the row being deleted. Referencing rows are
// found through an index on the FK columns when one exists, falling back to
// a full scan.
func (e *Engine) checkNoReferences(ctx context.Context, view kv.KV, tbl catalog.Table, row lir.Row) error {
	all, err := catalog.NewReader(view).ListTables(ctx)
	if err != nil {
		return err
	}
	for _, child := range all {
		for _, fk := range child.ForeignKeys {
			if fk.RefTableID != tbl.ID {
				continue
			}
			// The parent values the child would reference.
			want := make(lir.Row, len(fk.Columns))
			for i, childCol := range fk.Columns {
				want[childCol] = row[fk.RefColumns[i]]
			}
			found, err := e.anyRowMatching(ctx, view, child, fk.Columns, want, tbl, row)
			if err != nil {
				return err
			}
			if found {
				return fmt.Errorf("exec: cannot delete from %q: row is referenced by %q via %q", tbl.Name, child.Name, fk.Name)
			}
		}
	}
	return nil
}

// anyRowMatching reports whether child has any row whose cols equal want.
// Self-referential checks skip the row being deleted itself.
func (e *Engine) anyRowMatching(ctx context.Context, view kv.KV, child catalog.Table, cols []string, want lir.Row, parent catalog.Table, parentRow lir.Row) (bool, error) {
	selfPK := []byte(nil)
	if child.ID == parent.ID {
		pk, err := encodeRowTuple(parentRow, parent.PrimaryKey)
		if err != nil {
			return false, err
		}
		selfPK = pk
	}

	// Prefer an index whose leading columns cover the FK columns.
	for _, idx := range child.Indexes {
		if len(idx.Columns) < len(cols) || !slices.Equal(idx.Columns[:len(cols)], cols) {
			continue
		}
		prefixTuple, err := encodeRowTuple(want, cols)
		if err != nil {
			return false, err
		}
		keyPrefix := append(IndexPrefix(child.ID, idx.ID), prefixTuple...)
		it, err := view.Scan(ctx, keyPrefix, keyenc.PrefixEnd(keyPrefix))
		if err != nil {
			return false, err
		}
		defer it.Close()
		for it.Next() {
			if selfPK != nil && bytes.Equal(it.Value(), selfPK) {
				continue
			}
			return true, it.Err()
		}
		return false, it.Err()
	}

	// Fall back to a full table scan.
	it, err := scanTable(ctx, view, child)
	if err != nil {
		return false, err
	}
	defer it.Close()
	for {
		r, ok, err := it.Next()
		if err != nil || !ok {
			return false, err
		}
		match := true
		for _, c := range cols {
			if !r[c].Equal(want[c]) {
				match = false
				break
			}
		}
		if !match {
			continue
		}
		if selfPK != nil {
			pk, err := encodeRowTuple(r, child.PrimaryKey)
			if err != nil {
				return false, err
			}
			if bytes.Equal(pk, selfPK) {
				continue
			}
		}
		return true, nil
	}
}
