package mutate

import (
	"bytes"
	"context"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func checkForeignKeys(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row) error {
	for _, fk := range tbl.ForeignKeys {
		if err := checkForeignKey(ctx, view, row, fk); err != nil {
			return err
		}
	}
	return nil
}

func checkForeignKeysFor(ctx context.Context, view kv.KV, tbl model.Table, row, set lir.Row) error {
	for _, fk := range tbl.ForeignKeys {
		if touches(fk.Columns, set) {
			if err := checkForeignKey(ctx, view, row, fk); err != nil {
				return err
			}
		}
	}
	return nil
}

// A foreign key with any NULL component is satisfied. Parent reads join the
// transaction's read set, so a concurrent parent deletion conflicts at commit.
func checkForeignKey(ctx context.Context, view kv.KV, row lir.Row, fk model.ForeignKey) error {
	values := make([]lir.Value, len(fk.Columns))
	for i, name := range fk.Columns {
		values[i] = row[name]
		if values[i].Null {
			return nil
		}
	}
	parentTuple, err := codec.EncodeTuple(values)
	if err != nil {
		return err
	}
	_, ok, err := view.Get(ctx, codec.DataKey(fk.RefTableID, parentTuple))
	if err != nil {
		return err
	}
	if !ok {
		return reject.Inputf("exec: foreign key %q violation: referenced row does not exist", fk.Name)
	}
	return nil
}

func checkUniqueIndexes(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte) error {
	for _, idx := range tbl.Indexes {
		if err := checkUniqueIndex(ctx, view, tbl, row, pkTuple, idx); err != nil {
			return err
		}
	}
	return nil
}

func checkUniqueIndexesFor(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte, set lir.Row) error {
	for _, idx := range tbl.Indexes {
		if touches(idx.Columns, set) {
			if err := checkUniqueIndex(ctx, view, tbl, row, pkTuple, idx); err != nil {
				return err
			}
		}
	}
	return nil
}

// The full indexed tuple is a self-delimiting key prefix. Scanning that prefix
// also records an empty range read, which makes concurrent equal writes conflict.
func checkUniqueIndex(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row, pkTuple []byte, idx model.Index) error {
	if !idx.Unique || anyNullComponent(row, idx.Columns) {
		return nil
	}
	idxTuple, err := codec.EncodeRowTuple(row, idx.Columns)
	if err != nil {
		return err
	}
	prefix := append(codec.IndexPrefix(tbl.ID, idx.ID), idxTuple...)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return err
	}
	for it.Next() {
		if !bytes.Equal(it.Value(), pkTuple) {
			_ = it.Close()
			return reject.Inputf("exec: unique index %q violation in table %q", idx.Name, tbl.Name)
		}
	}
	err = it.Err()
	if closeErr := it.Close(); err == nil {
		err = closeErr
	}
	return err
}

func anyNullComponent(row lir.Row, columns []string) bool {
	for _, column := range columns {
		if row[column].Null {
			return true
		}
	}
	return false
}

func touches(columns []string, set lir.Row) bool {
	for _, column := range columns {
		if _, ok := set[column]; ok {
			return true
		}
	}
	return false
}

// Delete uses restrict semantics. An index whose leading columns match the
// foreign key avoids a full scan of the referencing table.
func checkNoReferences(ctx context.Context, view kv.KV, tbl model.Table, row lir.Row) error {
	tables, err := store.New(view).ListTables(ctx)
	if err != nil {
		return err
	}
	for _, child := range tables {
		for _, fk := range child.ForeignKeys {
			if fk.RefTableID != tbl.ID {
				continue
			}
			want := make(lir.Row, len(fk.Columns))
			for i, childColumn := range fk.Columns {
				want[childColumn] = row[fk.RefColumns[i]]
			}
			found, err := anyRowMatching(ctx, view, child, fk.Columns, want, tbl, row)
			if err != nil {
				return err
			}
			if found {
				return reject.Inputf("exec: cannot delete from %q: row is referenced by %q via %q", tbl.Name, child.Name, fk.Name)
			}
		}
	}
	return nil
}

// Self-referential checks ignore the row being deleted.
func anyRowMatching(ctx context.Context, view kv.KV, child model.Table, columns []string, want lir.Row, parent model.Table, parentRow lir.Row) (bool, error) {
	var selfPK []byte
	if child.ID == parent.ID {
		var err error
		selfPK, err = codec.EncodeRowTuple(parentRow, parent.PrimaryKey)
		if err != nil {
			return false, err
		}
	}

	for _, idx := range child.Indexes {
		if len(idx.Columns) < len(columns) || !slices.Equal(idx.Columns[:len(columns)], columns) {
			continue
		}
		prefixTuple, err := codec.EncodeRowTuple(want, columns)
		if err != nil {
			return false, err
		}
		prefix := append(codec.IndexPrefix(child.ID, idx.ID), prefixTuple...)
		it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
		if err != nil {
			return false, err
		}
		defer it.Close()
		for it.Next() {
			if selfPK != nil && bytes.Equal(it.Value(), selfPK) {
				continue
			}
			return true, nil
		}
		return false, it.Err()
	}

	it, err := rowstore.ScanTable(ctx, view, child)
	if err != nil {
		return false, err
	}
	defer it.Close()
	for {
		candidate, ok, err := it.Next()
		if err != nil || !ok {
			return false, err
		}
		match := true
		for _, column := range columns {
			if !candidate[column].Equal(want[column]) {
				match = false
				break
			}
		}
		if !match {
			continue
		}
		if selfPK != nil {
			pk, err := codec.EncodeRowTuple(candidate, child.PrimaryKey)
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
