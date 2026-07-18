package change

// Service methods open serializable transactions. Mutation methods apply the
// same changes to a caller-owned transaction so metadata and data work can
// commit atomically:
//
//   - Renames are free: rows are stored keyed by column ID and data keys by
//     table ID, so renaming updates nothing but the catalog.
//   - DeleteTable / DeleteIndex leave their data and index entries unreachable.
//     Physical IDs are never reused, so those entries cannot become visible
//     through another catalog object.
//   - CreateIndex records metadata only; backfilling entries for existing rows
//     is the executor's job. The two must commit together — a registered
//     index with no entries would let the planner return wrong results — so
//     the executor composes Mutation.CreateIndex with its backfill in one transaction.

import (
	"context"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/naming"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// mutateTableIn loads a table, applies fn, and saves the result, all against
// the given view — typically a transaction owned by the caller, so table
// mutations can commit atomically with other work (index backfills).
func mutateTableIn(ctx context.Context, view kv.KV, tableName string, fn func(view kv.KV, tbl *model.Table) error) (model.Table, error) {
	tbl, ok, err := store.New(view).GetTable(ctx, tableName)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("catalog: table %q does not exist", tableName)
	}
	if err := fn(view, &tbl); err != nil {
		return model.Table{}, err
	}
	return tbl, store.SaveTable(ctx, view, tbl)
}

func (m *Mutation) mutateTable(ctx context.Context, tableName string, fn func(view kv.KV, tbl *model.Table) error) (model.Table, error) {
	tbl, err := mutateTableIn(ctx, m.view, tableName, fn)
	if err != nil {
		return model.Table{}, err
	}
	m.changed = true
	return tbl, nil
}

// DeleteTable removes the table from the catalog. Row and index data become
// unreachable garbage (IDs are never reused). A table another table still
// references through a foreign key cannot be deleted — delete the referencing
// table first (self-references don't count; they die with the table).
func (c *Service) DeleteTable(ctx context.Context, tableName string) error {
	return c.mutate(ctx, func(change *Mutation) error {
		return change.DeleteTable(ctx, tableName)
	})
}

// DeleteTable removes a table inside this catalog change.
func (m *Mutation) DeleteTable(ctx context.Context, tableName string) error {
	id, ok, err := m.view.Get(ctx, store.TableNameKey(tableName))
	if err != nil {
		return err
	}
	if !ok {
		return reject.Inputf("catalog: table %q does not exist", tableName)
	}
	tables, err := store.New(m.view).ListTables(ctx)
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
	if err := m.view.Delete(ctx, store.TableKey(string(id))); err != nil {
		return err
	}
	if err := m.view.Delete(ctx, store.TableNameKey(tableName)); err != nil {
		return err
	}
	m.changed = true
	return nil
}

// RenameTable changes a table's name. Data keys use the table ID, so this
// is metadata-only.
func (c *Service) RenameTable(ctx context.Context, oldName, newName string) error {
	return c.mutate(ctx, func(change *Mutation) error {
		return change.RenameTable(ctx, oldName, newName)
	})
}

// RenameTable renames a table inside this catalog change.
func (m *Mutation) RenameTable(ctx context.Context, oldName, newName string) error {
	if oldName == newName {
		return nil
	}
	oldKey := store.TableNameKey(oldName)
	newKey := store.TableNameKey(newName)
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

	tbl, ok, err := store.New(m.view).GetTableByID(ctx, string(id))
	if err != nil || !ok {
		return reject.Inputf("catalog: table %q metadata missing (%v)", oldName, err)
	}
	for i := range tbl.Indexes {
		index := &tbl.Indexes[i]
		if index.Name == naming.Index(oldName, index.Columns, index.Unique) {
			index.Name = naming.Index(newName, index.Columns, index.Unique)
		}
	}
	for i := range tbl.ForeignKeys {
		foreignKey := &tbl.ForeignKeys[i]
		if len(foreignKey.Columns) == 1 && foreignKey.Name == naming.ForeignKey(oldName, foreignKey.Columns[0]) {
			foreignKey.Name = naming.ForeignKey(newName, foreignKey.Columns[0])
		}
	}
	tbl.Name = newName
	if err := store.SaveTable(ctx, m.view, tbl); err != nil {
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
func (c *Service) CreateColumn(ctx context.Context, tableName string, def model.ColumnDef) (model.Table, error) {
	var out model.Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.CreateColumn(ctx, tableName, def)
		return err
	})
	return out, err
}

// CreateColumn appends a column inside this catalog change.
func (m *Mutation) CreateColumn(ctx context.Context, tableName string, def model.ColumnDef) (model.Table, error) {
	return m.mutateTable(ctx, tableName, func(view kv.KV, tbl *model.Table) error {
		var err error
		def, err = assignColumnDefinitionID(ctx, view, *tbl, def)
		if err != nil {
			return err
		}
		if _, exists := tbl.Column(def.Name); exists {
			return reject.Inputf("catalog: column %q already exists in table %q", def.Name, tableName)
		}
		if err := validateColumnDef(def); err != nil {
			return err
		}
		if !def.Nullable && (def.Default == nil || def.Default.Func != "") {
			return reject.Inputf("catalog: new column %q must be nullable or have a literal default (existing rows need a value)", def.Name)
		}
		column, err := buildColumn(ctx, view, def)
		if err != nil {
			return err
		}
		tbl.Columns = append(tbl.Columns, column)
		return nil
	})
}

// DeleteColumn removes a column. The column must not be part of the primary
// key, any index, or any foreign key — delete those first.
func (c *Service) DeleteColumn(ctx context.Context, tableName, colName string) (model.Table, error) {
	var out model.Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.DeleteColumn(ctx, tableName, colName)
		return err
	})
	return out, err
}

// DeleteColumn removes a column inside this catalog change.
func (m *Mutation) DeleteColumn(ctx context.Context, tableName, colName string) (model.Table, error) {
	return m.mutateTable(ctx, tableName, func(view kv.KV, tbl *model.Table) error {
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
		tbl.Columns = slices.DeleteFunc(tbl.Columns, func(c model.Column) bool { return c.Name == colName })
		return nil
	})
}

// RenameColumn changes a column's name and rewrites every metadata
// reference to it (primary key, indexes, foreign keys). Rows are keyed by
// column ID, so no data is touched.
func (c *Service) RenameColumn(ctx context.Context, tableName, oldName, newName string) (model.Table, error) {
	var out model.Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.RenameColumn(ctx, tableName, oldName, newName)
		return err
	})
	return out, err
}

// RenameColumn renames a column inside this catalog change.
func (m *Mutation) RenameColumn(ctx context.Context, tableName, oldName, newName string) (model.Table, error) {
	if oldName == newName {
		tbl, ok, err := store.New(m.view).GetTable(ctx, tableName)
		if err != nil {
			return model.Table{}, err
		}
		if !ok {
			return model.Table{}, reject.Inputf("catalog: table %q does not exist", tableName)
		}
		return tbl, nil
	}
	renamed, err := m.mutateTable(ctx, tableName, func(view kv.KV, tbl *model.Table) error {
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
		for i := range tbl.Indexes {
			index := &tbl.Indexes[i]
			generated := index.Name == naming.Index(tableName, index.Columns, index.Unique)
			rename(index.Columns)
			if generated {
				index.Name = naming.Index(tableName, index.Columns, index.Unique)
			}
		}
		for i := range tbl.ForeignKeys {
			foreignKey := &tbl.ForeignKeys[i]
			generated := len(foreignKey.Columns) == 1 &&
				foreignKey.Name == naming.ForeignKey(tableName, foreignKey.Columns[0])
			rename(foreignKey.Columns)
			if generated {
				foreignKey.Name = naming.ForeignKey(tableName, foreignKey.Columns[0])
			}
			if foreignKey.RefTableID == tbl.ID {
				rename(foreignKey.RefColumns)
			}
		}
		for i := range tbl.Columns {
			if tbl.Columns[i].Name == oldName {
				tbl.Columns[i].Name = newName
			}
		}
		rename(tbl.PrimaryKey)
		return nil
	})
	if err != nil {
		return model.Table{}, err
	}

	tables, err := store.New(m.view).ListTables(ctx)
	if err != nil {
		return model.Table{}, err
	}
	for _, table := range tables {
		if table.ID == renamed.ID {
			continue
		}
		changed := false
		for i := range table.ForeignKeys {
			foreignKey := &table.ForeignKeys[i]
			if foreignKey.RefTableID != renamed.ID {
				continue
			}
			for j, refColumn := range foreignKey.RefColumns {
				if refColumn == oldName {
					foreignKey.RefColumns[j] = newName
					changed = true
				}
			}
		}
		if changed {
			if err := store.SaveTable(ctx, m.view, table); err != nil {
				return model.Table{}, err
			}
		}
	}
	return renamed, nil
}

// createIndexIn records a new index in the catalog against the caller's view
// and returns the updated table plus the new index. Entries for existing
// rows are NOT written here — the executor backfills them, in the same
// transaction, so the registration and its entries commit or fail together
// (a registered index with no entries would silently drop rows from reads).
func createIndexIn(ctx context.Context, view kv.KV, tableName string, def model.IndexDef) (model.Table, model.Index, error) {
	var added model.Index
	tbl, err := mutateTableIn(ctx, view, tableName, func(view kv.KV, tbl *model.Table) error {
		if _, exists := tbl.Index(def.Name); exists {
			return reject.Inputf("catalog: index %q already exists on table %q", def.Name, tableName)
		}
		var err error
		added, err = buildIndex(ctx, view, *tbl, def)
		if err == nil {
			tbl.Indexes = append(tbl.Indexes, added)
		}
		return err
	})
	if err != nil {
		return model.Table{}, model.Index{}, err
	}
	return tbl, added, nil
}

// CreateIndex is createIndexIn in its own catalog transaction, for indexes over tables
// with no existing rows to backfill.
func (c *Service) CreateIndex(ctx context.Context, tableName string, def model.IndexDef) (model.Index, error) {
	var added model.Index
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		_, added, err = change.CreateIndex(ctx, tableName, def)
		return err
	})
	if err != nil {
		return model.Index{}, err
	}
	return added, nil
}

// CreateIndex records an index inside this catalog change. The caller is
// responsible for backfilling existing rows in the same transaction.
func (m *Mutation) CreateIndex(ctx context.Context, tableName string, def model.IndexDef) (model.Table, model.Index, error) {
	tbl, added, err := createIndexIn(ctx, m.view, tableName, def)
	if err != nil {
		return model.Table{}, model.Index{}, err
	}
	m.changed = true
	return tbl, added, nil
}

// DeleteIndex removes an index from the catalog. Its entries become
// unreachable garbage (index IDs are never reused).
func (c *Service) DeleteIndex(ctx context.Context, tableName, indexName string) error {
	return c.mutate(ctx, func(change *Mutation) error {
		return change.DeleteIndex(ctx, tableName, indexName)
	})
}

// DeleteIndex removes an index inside this catalog change.
func (m *Mutation) DeleteIndex(ctx context.Context, tableName, indexName string) error {
	_, err := m.mutateTable(ctx, tableName, func(view kv.KV, tbl *model.Table) error {
		if _, ok := tbl.Index(indexName); !ok {
			return reject.Inputf("catalog: index %q does not exist on table %q", indexName, tableName)
		}
		tbl.Indexes = slices.DeleteFunc(tbl.Indexes, func(i model.Index) bool { return i.Name == indexName })
		return nil
	})
	return err
}
