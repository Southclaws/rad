// Package change applies catalog mutations and records schema revisions.
package change

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Service reads and writes table metadata in the KV store. A Rad instance
// is exactly one database — there is no schema or database hierarchy; two
// databases are two Rad deployments. Direct mutations each run in a
// SerializableSnapshot transaction; schema migrations group the same
// Mutation operations in one caller-owned transaction. Service metadata,
// revision history, and associated index backfills therefore commit
// atomically, while concurrent schema changes conflict instead of corrupting
// either counter.
type Service struct {
	store kv.TransactionalKV
}

func New(store kv.TransactionalKV) *Service {
	return &Service{store: store}
}

// transact runs fn in a transaction and commits it if fn returns nil. It is
// for catalog metadata operations which are not schema changes, such as
// settling the catalog mode. model.Schema changes use mutate, which also records a
// revision in the same transaction.
func (c *Service) transact(ctx context.Context, fn func(view kv.Txn) error) error {
	txn, err := c.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	if err := fn(txn); err != nil {
		return err
	}
	return txn.Commit(ctx)
}

// mutate runs one catalog change in a transaction. However many low-level
// operations fn performs, a successful change records exactly one revision.
func (c *Service) mutate(ctx context.Context, fn func(change *Mutation) error) error {
	return c.transact(ctx, func(view kv.Txn) error {
		_, err := Apply(ctx, view, fn)
		return err
	})
}

func (c *Service) CreateTable(ctx context.Context, def model.TableDef) (model.Table, error) {
	var tbl model.Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		tbl, err = change.CreateTable(ctx, def)
		return err
	})
	if err != nil {
		return model.Table{}, err
	}
	return tbl, nil
}

// CreateTable defines a table inside this catalog change.
func (m *Mutation) CreateTable(ctx context.Context, def model.TableDef) (model.Table, error) {
	tbl, err := createTable(ctx, m.view, def)
	if err != nil {
		return model.Table{}, err
	}
	m.changed = true
	return tbl, nil
}

func createTable(ctx context.Context, view kv.KV, def model.TableDef) (model.Table, error) {
	def, err := assignTableDefinitionIDs(ctx, view, def)
	if err != nil {
		return model.Table{}, err
	}
	if def.Name == "" {
		return model.Table{}, reject.Inputf("catalog: table name is required")
	}
	nameKey := store.TableNameKey(def.Name)
	if _, ok, err := view.Get(ctx, nameKey); err != nil {
		return model.Table{}, err
	} else if ok {
		return model.Table{}, reject.Inputf("catalog: table %q already exists", def.Name)
	}

	tbl := model.Table{SchemaID: def.ID, Name: def.Name}
	tbl.ID, err = store.NextPhysicalID(ctx, view, "t")
	if err != nil {
		return model.Table{}, err
	}

	seen := map[string]bool{}
	for _, cd := range def.Columns {
		if seen[cd.Name] {
			return model.Table{}, reject.Inputf("catalog: duplicate column %q", cd.Name)
		}
		seen[cd.Name] = true
		if err := validateColumnDef(cd); err != nil {
			return model.Table{}, err
		}
		column, err := buildColumn(ctx, view, cd)
		if err != nil {
			return model.Table{}, err
		}
		tbl.Columns = append(tbl.Columns, column)
	}

	if len(def.PrimaryKey) == 0 {
		return model.Table{}, reject.Inputf("catalog: table %q needs a primary key", def.Name)
	}
	for _, name := range def.PrimaryKey {
		col, ok := tbl.Column(name)
		if !ok {
			return model.Table{}, reject.Inputf("catalog: primary key column %q does not exist", name)
		}
		if col.Nullable {
			return model.Table{}, reject.Inputf("catalog: primary key column %q must not be nullable", name)
		}
	}
	tbl.PrimaryKey = def.PrimaryKey

	for _, index := range def.Indexes {
		created, err := buildIndex(ctx, view, tbl, index)
		if err != nil {
			return model.Table{}, err
		}
		tbl.Indexes = append(tbl.Indexes, created)
	}

	for _, fd := range def.ForeignKeys {
		var ref model.Table
		if fd.RefTable == def.Name {
			// Self-referential foreign key: validate against the table
			// being created.
			ref = tbl
			ref.PrimaryKey = def.PrimaryKey
		} else {
			var ok bool
			var err error
			ref, ok, err = store.New(view).GetTable(ctx, fd.RefTable)
			if err != nil {
				return model.Table{}, err
			}
			if !ok {
				return model.Table{}, reject.Inputf("catalog: foreign key %q references unknown table %q", fd.Name, fd.RefTable)
			}
		}
		// Requiring the full primary key keeps every reference aligned with
		// the parent row identity used by storage and constraint checks.
		if len(fd.RefColumns) != len(ref.PrimaryKey) {
			return model.Table{}, reject.Inputf("catalog: foreign key %q must reference %q's primary key", fd.Name, fd.RefTable)
		}
		for i, name := range fd.RefColumns {
			if name != ref.PrimaryKey[i] {
				return model.Table{}, reject.Inputf("catalog: foreign key %q must reference %q's primary key", fd.Name, fd.RefTable)
			}
		}
		if len(fd.Columns) != len(fd.RefColumns) {
			return model.Table{}, reject.Inputf("catalog: foreign key %q column count mismatch", fd.Name)
		}
		for i, name := range fd.Columns {
			col, ok := tbl.Column(name)
			if !ok {
				return model.Table{}, reject.Inputf("catalog: foreign key %q references unknown column %q", fd.Name, name)
			}
			refCol, _ := ref.Column(fd.RefColumns[i])
			if col.Type != refCol.Type {
				return model.Table{}, reject.Inputf("catalog: foreign key %q type mismatch on %q", fd.Name, name)
			}
		}
		fid, err := store.NextPhysicalID(ctx, view, "fk")
		if err != nil {
			return model.Table{}, err
		}
		tbl.ForeignKeys = append(tbl.ForeignKeys, model.ForeignKey{
			ID: fid, Name: fd.Name, Columns: fd.Columns,
			RefTableID: ref.ID, RefColumns: fd.RefColumns,
		})
	}

	if err := store.SaveTable(ctx, view, tbl); err != nil {
		return model.Table{}, err
	}
	if err := view.Put(ctx, nameKey, []byte(tbl.ID)); err != nil {
		return model.Table{}, err
	}
	return tbl, nil
}

func (c *Service) GetTable(ctx context.Context, tableName string) (model.Table, bool, error) {
	return store.New(c.store).GetTable(ctx, tableName)
}

func (c *Service) GetTableByID(ctx context.Context, id string) (model.Table, bool, error) {
	return store.New(c.store).GetTableByID(ctx, id)
}

// GetTableBySchemaID resolves a table by its stable logical identity.
func (c *Service) GetTableBySchemaID(ctx context.Context, id model.SchemaID) (model.Table, bool, error) {
	return store.New(c.store).GetTableBySchemaID(ctx, id)
}

// ListTables returns every table in the database, sorted by name.
func (c *Service) ListTables(ctx context.Context) ([]model.Table, error) {
	return store.New(c.store).ListTables(ctx)
}
