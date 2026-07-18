package catalogapi

// The name-based direct catalog façade. Each adapter resolves mutable resource
// names to stable schema identities, constructs one catalog Program statement,
// and executes it with a per-statement revision boundary. This keeps the
// convenience API and PIR on one mutation path. Catalog-mode enforcement
// remains a transport concern.

import (
	"context"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/reject"
)

type Executor interface {
	ExecuteProgram(context.Context, execprogram.Program, execprogram.Options) (execprogram.Result, error)
}

type Service struct {
	cat  *catalog.Catalog
	exec Executor
}

func New(cat *catalog.Catalog, executor Executor) *Service {
	return &Service{cat: cat, exec: executor}
}

// CreateTable defines a new table.
func (db *Service) CreateTable(ctx context.Context, def model.TableDef) (model.Table, error) {
	err := db.executeCatalog(ctx, execprogram.Statement{
		Name: "create_table", Kind: execprogram.CreateTable, TableDef: def,
	})
	if err != nil {
		return model.Table{}, err
	}
	table, ok, err := db.cat.GetTable(ctx, def.Name)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("frontend: created table %q is missing", def.Name)
	}
	return table, nil
}

// DeleteTable removes a table from the catalog. Tables referenced by other
// tables' foreign keys must have their referencing tables deleted first.
func (db *Service) DeleteTable(ctx context.Context, table string) error {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, execprogram.Statement{
		Name: "delete_table", Kind: execprogram.DeleteTable, TableID: resolved.SchemaID,
	})
}

// RenameTable changes a table's name; metadata-only, data keys are by ID.
func (db *Service) RenameTable(ctx context.Context, from, to string) error {
	table, err := db.table(ctx, from)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, execprogram.Statement{
		Name: "rename_table", Kind: execprogram.RenameTable, TableID: table.SchemaID, To: to,
	})
}

// CreateColumn appends a column. It must be nullable or carry a literal
// default, since existing rows need a value.
func (db *Service) CreateColumn(ctx context.Context, table string, def model.ColumnDef) (model.Table, error) {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return model.Table{}, err
	}
	err = db.executeCatalog(ctx, execprogram.Statement{
		Name: "create_column", Kind: execprogram.CreateColumn,
		TableID: resolved.SchemaID, Column: def,
	})
	if err != nil {
		return model.Table{}, err
	}
	return db.tableBySchemaID(ctx, resolved.SchemaID)
}

// DeleteColumn removes a column not used by the primary key, an index, or a
// foreign key.
func (db *Service) DeleteColumn(ctx context.Context, table, column string) (model.Table, error) {
	resolved, col, err := db.column(ctx, table, column)
	if err != nil {
		return model.Table{}, err
	}
	err = db.executeCatalog(ctx, execprogram.Statement{
		Name: "delete_column", Kind: execprogram.DeleteColumn,
		TableID: resolved.SchemaID, ColumnID: col.SchemaID,
	})
	if err != nil {
		return model.Table{}, err
	}
	return db.tableBySchemaID(ctx, resolved.SchemaID)
}

// RenameColumn changes a column's name everywhere it appears in metadata.
func (db *Service) RenameColumn(ctx context.Context, table, from, to string) (model.Table, error) {
	resolved, column, err := db.column(ctx, table, from)
	if err != nil {
		return model.Table{}, err
	}
	err = db.executeCatalog(ctx, execprogram.Statement{
		Name: "rename_column", Kind: execprogram.RenameColumn,
		TableID: resolved.SchemaID, ColumnID: column.SchemaID, To: to,
	})
	if err != nil {
		return model.Table{}, err
	}
	return db.tableBySchemaID(ctx, resolved.SchemaID)
}

// CreateIndex registers an index and backfills entries for existing rows in one
// transaction — the registration never becomes visible without its entries
// (a unique violation in existing data rolls both back).
func (db *Service) CreateIndex(ctx context.Context, table string, def model.IndexDef) error {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, execprogram.Statement{
		Name: "create_index", Kind: execprogram.CreateIndex,
		TableID: resolved.SchemaID, Index: def,
	})
}

// DeleteIndex removes an index; its entries become unreachable garbage.
func (db *Service) DeleteIndex(ctx context.Context, table, index string) error {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, execprogram.Statement{
		Name: "delete_index", Kind: execprogram.DeleteIndex,
		TableID: resolved.SchemaID, IndexName: index,
	})
}

func (db *Service) executeCatalog(ctx context.Context, statement execprogram.Statement) error {
	_, err := db.exec.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{statement}}, execprogram.Options{
		Catalog: execprogram.CatalogRevisionPerStatement,
	})
	return err
}

func (db *Service) table(ctx context.Context, name string) (model.Table, error) {
	table, ok, err := db.cat.GetTable(ctx, name)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("catalog: table %q does not exist", name)
	}
	return table, nil
}

func (db *Service) tableBySchemaID(ctx context.Context, id model.SchemaID) (model.Table, error) {
	table, ok, err := db.cat.GetTableBySchemaID(ctx, id)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("catalog: table schema ID %d does not exist", id)
	}
	return table, nil
}

func (db *Service) column(ctx context.Context, tableName, columnName string) (model.Table, model.Column, error) {
	table, err := db.table(ctx, tableName)
	if err != nil {
		return model.Table{}, model.Column{}, err
	}
	column, ok := table.Column(columnName)
	if !ok {
		return model.Table{}, model.Column{}, reject.Inputf(
			"catalog: column %q does not exist in table %q", columnName, tableName)
	}
	return table, column, nil
}
