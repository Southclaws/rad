package catalog

// Schema-evolution operations. Each runs in its own serializable
// transaction, mutating table metadata only:
//
//   - Renames are free: rows are stored keyed by column ID and data keys by
//     table ID, so renaming updates nothing but the catalog.
//   - DeleteTable / DeleteIndex orphan their data and index entries. IDs are
//     never reused, so orphans are unreachable garbage awaiting a future
//     vacuum — acceptable for the POC.
//   - CreateIndex records metadata only; backfilling entries for existing rows
//     is the executor's job. The two must commit together — a registered
//     index with no entries would let the planner return wrong results — so
//     the executor composes Mutation.CreateIndex with its backfill in one transaction.

import (
	"context"
	"encoding/json"
	"github.com/Southclaws/rad/rad/engine/reject"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
)

// saveTable persists an updated table definition.
func saveTable(ctx context.Context, view kv.KV, tbl Table) error {
	raw, err := json.Marshal(tbl)
	if err != nil {
		return err
	}
	return view.Put(ctx, []byte(tablePrefix+tbl.ID), raw)
}

// mutateTableIn loads a table, applies fn, and saves the result, all against
// the given view — typically a transaction owned by the caller, so table
// mutations can commit atomically with other work (index backfills).
func mutateTableIn(ctx context.Context, view kv.KV, tableName string, fn func(view kv.KV, tbl *Table) error) (Table, error) {
	id, ok, err := view.Get(ctx, []byte(tableNamePrefix+tableName))
	if err != nil {
		return Table{}, err
	}
	if !ok {
		return Table{}, reject.Inputf("catalog: table %q does not exist", tableName)
	}
	raw, ok, err := view.Get(ctx, []byte(tablePrefix+string(id)))
	if err != nil {
		return Table{}, err
	}
	if !ok {
		return Table{}, reject.Inputf("catalog: table %q metadata missing", tableName)
	}
	var tbl Table
	if err := json.Unmarshal(raw, &tbl); err != nil {
		return Table{}, err
	}
	if err := fn(view, &tbl); err != nil {
		return Table{}, err
	}
	return tbl, saveTable(ctx, view, tbl)
}

func (m *Mutation) mutateTable(ctx context.Context, tableName string, fn func(view kv.KV, tbl *Table) error) (Table, error) {
	tbl, err := mutateTableIn(ctx, m.view, tableName, fn)
	if err != nil {
		return Table{}, err
	}
	m.changed = true
	return tbl, nil
}

// DeleteTable removes the table from the catalog. Row and index data become
// unreachable garbage (IDs are never reused). A table another table still
// references through a foreign key cannot be deleted — delete the referencing
// table first (self-references don't count; they die with the table).
func (c *Catalog) DeleteTable(ctx context.Context, tableName string) error {
	return c.mutate(ctx, func(change *Mutation) error {
		return change.DeleteTable(ctx, tableName)
	})
}

// DeleteTable removes a table inside this catalog change.
func (m *Mutation) DeleteTable(ctx context.Context, tableName string) error {
	id, ok, err := m.view.Get(ctx, []byte(tableNamePrefix+tableName))
	if err != nil {
		return err
	}
	if !ok {
		return reject.Inputf("catalog: table %q does not exist", tableName)
	}
	tables, err := listTables(ctx, m.view)
	if err != nil {
		return err
	}
	for _, t := range tables {
		if t.ID == string(id) {
			continue
		}
		for _, fk := range t.ForeignKeys {
			if fk.RefTableID == string(id) {
				return reject.Inputf("catalog: table %q is referenced by foreign key %q on table %q; delete that table first", tableName, fk.Name, t.Name)
			}
		}
	}
	if err := m.view.Delete(ctx, []byte(tablePrefix+string(id))); err != nil {
		return err
	}
	if err := m.view.Delete(ctx, []byte(tableNamePrefix+tableName)); err != nil {
		return err
	}
	m.changed = true
	return nil
}

// RenameTable changes a table's name. Data keys use the table ID, so this
// is metadata-only.
func (c *Catalog) RenameTable(ctx context.Context, oldName, newName string) error {
	return c.mutate(ctx, func(change *Mutation) error {
		return change.RenameTable(ctx, oldName, newName)
	})
}

// RenameTable renames a table inside this catalog change.
func (m *Mutation) RenameTable(ctx context.Context, oldName, newName string) error {
	if oldName == newName {
		return nil
	}
	oldKey := []byte(tableNamePrefix + oldName)
	newKey := []byte(tableNamePrefix + newName)
	id, ok, err := m.view.Get(ctx, oldKey)
	if err != nil {
		return err
	}
	if !ok {
		return reject.Inputf("catalog: table %q does not exist", oldName)
	}
	if _, taken, err := m.view.Get(ctx, newKey); err != nil {
		return err
	} else if taken {
		return reject.Inputf("catalog: table %q already exists", newName)
	}

	raw, ok, err := m.view.Get(ctx, []byte(tablePrefix+string(id)))
	if err != nil || !ok {
		return reject.Inputf("catalog: table %q metadata missing (%v)", oldName, err)
	}
	var tbl Table
	if err := json.Unmarshal(raw, &tbl); err != nil {
		return err
	}
	tbl.Name = newName
	if err := saveTable(ctx, m.view, tbl); err != nil {
		return err
	}
	if err := m.view.Delete(ctx, oldKey); err != nil {
		return err
	}
	if err := m.view.Put(ctx, newKey, id); err != nil {
		return err
	}
	m.changed = true
	return nil
}

// CreateColumn appends a column. Because existing rows lack the column, it
// must be nullable or carry a literal default (generator defaults would
// yield a different value on every read of an old row).
func (c *Catalog) CreateColumn(ctx context.Context, tableName string, def ColumnDef) (Table, error) {
	var out Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.CreateColumn(ctx, tableName, def)
		return err
	})
	return out, err
}

// CreateColumn appends a column inside this catalog change.
func (m *Mutation) CreateColumn(ctx context.Context, tableName string, def ColumnDef) (Table, error) {
	return m.mutateTable(ctx, tableName, func(view kv.KV, tbl *Table) error {
		if _, exists := tbl.Column(def.Name); exists {
			return reject.Inputf("catalog: column %q already exists in table %q", def.Name, tableName)
		}
		switch def.Type {
		case TypeText, TypeInt64, TypeFloat64, TypeBool:
		default:
			return reject.Inputf("catalog: column %q has unsupported type %q", def.Name, def.Type)
		}
		if err := validateDefault(def); err != nil {
			return err
		}
		if !def.Nullable && (def.Default == nil || def.Default.Func != "") {
			return reject.Inputf("catalog: new column %q must be nullable or have a literal default (existing rows need a value)", def.Name)
		}
		id, err := nextID(ctx, view, "c")
		if err != nil {
			return err
		}
		tbl.Columns = append(tbl.Columns, Column{
			ID: id, Name: def.Name, Type: def.Type, Nullable: def.Nullable,
			Format: def.Format, Default: def.Default,
		})
		return nil
	})
}

// DeleteColumn removes a column. The column must not be part of the primary
// key, any index, or any foreign key — delete those first.
func (c *Catalog) DeleteColumn(ctx context.Context, tableName, colName string) (Table, error) {
	var out Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.DeleteColumn(ctx, tableName, colName)
		return err
	})
	return out, err
}

// DeleteColumn removes a column inside this catalog change.
func (m *Mutation) DeleteColumn(ctx context.Context, tableName, colName string) (Table, error) {
	return m.mutateTable(ctx, tableName, func(view kv.KV, tbl *Table) error {
		if _, ok := tbl.Column(colName); !ok {
			return reject.Inputf("catalog: column %q does not exist in table %q", colName, tableName)
		}
		if slices.Contains(tbl.PrimaryKey, colName) {
			return reject.Inputf("catalog: cannot delete primary key column %q", colName)
		}
		for _, idx := range tbl.Indexes {
			if slices.Contains(idx.Columns, colName) {
				return reject.Inputf("catalog: column %q is used by index %q; delete the index first", colName, idx.Name)
			}
		}
		for _, fk := range tbl.ForeignKeys {
			if slices.Contains(fk.Columns, colName) {
				return reject.Inputf("catalog: column %q is used by foreign key %q; delete the foreign key first", colName, fk.Name)
			}
		}
		tbl.Columns = slices.DeleteFunc(tbl.Columns, func(c Column) bool { return c.Name == colName })
		return nil
	})
}

// RenameColumn changes a column's name and rewrites every metadata
// reference to it (primary key, indexes, foreign keys). Rows are keyed by
// column ID, so no data is touched.
func (c *Catalog) RenameColumn(ctx context.Context, tableName, oldName, newName string) (Table, error) {
	var out Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.RenameColumn(ctx, tableName, oldName, newName)
		return err
	})
	return out, err
}

// RenameColumn renames a column inside this catalog change.
func (m *Mutation) RenameColumn(ctx context.Context, tableName, oldName, newName string) (Table, error) {
	if oldName == newName {
		tbl, ok, err := getTable(ctx, m.view, tableName)
		if err != nil {
			return Table{}, err
		}
		if !ok {
			return Table{}, reject.Inputf("catalog: table %q does not exist", tableName)
		}
		return tbl, nil
	}
	return m.mutateTable(ctx, tableName, func(view kv.KV, tbl *Table) error {
		if _, ok := tbl.Column(oldName); !ok {
			return reject.Inputf("catalog: column %q does not exist in table %q", oldName, tableName)
		}
		if _, exists := tbl.Column(newName); exists {
			return reject.Inputf("catalog: column %q already exists in table %q", newName, tableName)
		}
		rename := func(names []string) {
			for i, n := range names {
				if n == oldName {
					names[i] = newName
				}
			}
		}
		for i := range tbl.Columns {
			if tbl.Columns[i].Name == oldName {
				tbl.Columns[i].Name = newName
			}
		}
		rename(tbl.PrimaryKey)
		for i := range tbl.Indexes {
			rename(tbl.Indexes[i].Columns)
		}
		for i := range tbl.ForeignKeys {
			rename(tbl.ForeignKeys[i].Columns)
		}
		return nil
	})
}

// createIndexIn records a new index in the catalog against the caller's view
// and returns the updated table plus the new index. Entries for existing
// rows are NOT written here — the executor backfills them, in the same
// transaction, so the registration and its entries commit or fail together
// (a registered index with no entries would silently drop rows from reads).
func createIndexIn(ctx context.Context, view kv.KV, tableName string, def IndexDef) (Table, Index, error) {
	var added Index
	tbl, err := mutateTableIn(ctx, view, tableName, func(view kv.KV, tbl *Table) error {
		if _, exists := tbl.Index(def.Name); exists {
			return reject.Inputf("catalog: index %q already exists on table %q", def.Name, tableName)
		}
		if len(def.Columns) == 0 {
			return reject.Inputf("catalog: index %q has no columns", def.Name)
		}
		for _, col := range def.Columns {
			if _, ok := tbl.Column(col); !ok {
				return reject.Inputf("catalog: index %q references unknown column %q", def.Name, col)
			}
		}
		id, err := nextID(ctx, view, "i")
		if err != nil {
			return err
		}
		added = Index{ID: id, Name: def.Name, Columns: def.Columns, Unique: def.Unique}
		tbl.Indexes = append(tbl.Indexes, added)
		return nil
	})
	if err != nil {
		return Table{}, Index{}, err
	}
	return tbl, added, nil
}

// CreateIndex is createIndexIn in its own catalog transaction, for indexes over tables
// with no existing rows to backfill.
func (c *Catalog) CreateIndex(ctx context.Context, tableName string, def IndexDef) (Index, error) {
	var added Index
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		_, added, err = change.CreateIndex(ctx, tableName, def)
		return err
	})
	if err != nil {
		return Index{}, err
	}
	return added, nil
}

// CreateIndex records an index inside this catalog change. The caller is
// responsible for backfilling existing rows in the same transaction.
func (m *Mutation) CreateIndex(ctx context.Context, tableName string, def IndexDef) (Table, Index, error) {
	tbl, added, err := createIndexIn(ctx, m.view, tableName, def)
	if err != nil {
		return Table{}, Index{}, err
	}
	m.changed = true
	return tbl, added, nil
}

// DeleteIndex removes an index from the catalog. Its entries become
// unreachable garbage (index IDs are never reused).
func (c *Catalog) DeleteIndex(ctx context.Context, tableName, indexName string) error {
	return c.mutate(ctx, func(change *Mutation) error {
		return change.DeleteIndex(ctx, tableName, indexName)
	})
}

// DeleteIndex removes an index inside this catalog change.
func (m *Mutation) DeleteIndex(ctx context.Context, tableName, indexName string) error {
	_, err := m.mutateTable(ctx, tableName, func(view kv.KV, tbl *Table) error {
		if _, ok := tbl.Index(indexName); !ok {
			return reject.Inputf("catalog: index %q does not exist on table %q", indexName, tableName)
		}
		tbl.Indexes = slices.DeleteFunc(tbl.Indexes, func(i Index) bool { return i.Name == indexName })
		return nil
	})
	return err
}
