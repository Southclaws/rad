package frontend

// The name-based direct catalog façade. Each adapter resolves mutable resource
// names to stable schema identities, constructs one catalog Program statement,
// and executes it with a per-statement revision boundary. This keeps the
// convenience API and PIR on one mutation path. Catalog-mode enforcement
// remains a transport concern.

import (
	"context"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// CreateTable defines a new table.
func (db *DB) CreateTable(ctx context.Context, def catalog.TableDef) (catalog.Table, error) {
	err := db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "create_table", Kind: exec.StmtCreateTable, TableDef: def,
	})
	if err != nil {
		return catalog.Table{}, err
	}
	table, ok, err := db.cat.GetTable(ctx, def.Name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, reject.Inputf("frontend: created table %q is missing", def.Name)
	}
	return table, nil
}

// DeleteTable removes a table from the catalog. Tables referenced by other
// tables' foreign keys must have their referencing tables deleted first.
func (db *DB) DeleteTable(ctx context.Context, table string) error {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "delete_table", Kind: exec.StmtDeleteTable, TableID: resolved.SchemaID,
	})
}

// RenameTable changes a table's name; metadata-only, data keys are by ID.
func (db *DB) RenameTable(ctx context.Context, from, to string) error {
	table, err := db.table(ctx, from)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "rename_table", Kind: exec.StmtRenameTable, TableID: table.SchemaID, To: to,
	})
}

// CreateColumn appends a column. It must be nullable or carry a literal
// default, since existing rows need a value.
func (db *DB) CreateColumn(ctx context.Context, table string, def catalog.ColumnDef) (catalog.Table, error) {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return catalog.Table{}, err
	}
	err = db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "create_column", Kind: exec.StmtCreateColumn,
		TableID: resolved.SchemaID, Column: def,
	})
	if err != nil {
		return catalog.Table{}, err
	}
	return db.tableBySchemaID(ctx, resolved.SchemaID)
}

// DeleteColumn removes a column not used by the primary key, an index, or a
// foreign key.
func (db *DB) DeleteColumn(ctx context.Context, table, column string) (catalog.Table, error) {
	resolved, col, err := db.column(ctx, table, column)
	if err != nil {
		return catalog.Table{}, err
	}
	err = db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "delete_column", Kind: exec.StmtDeleteColumn,
		TableID: resolved.SchemaID, ColumnID: col.SchemaID,
	})
	if err != nil {
		return catalog.Table{}, err
	}
	return db.tableBySchemaID(ctx, resolved.SchemaID)
}

// RenameColumn changes a column's name everywhere it appears in metadata.
func (db *DB) RenameColumn(ctx context.Context, table, from, to string) (catalog.Table, error) {
	resolved, column, err := db.column(ctx, table, from)
	if err != nil {
		return catalog.Table{}, err
	}
	err = db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "rename_column", Kind: exec.StmtRenameColumn,
		TableID: resolved.SchemaID, ColumnID: column.SchemaID, To: to,
	})
	if err != nil {
		return catalog.Table{}, err
	}
	return db.tableBySchemaID(ctx, resolved.SchemaID)
}

// CreateIndex registers an index and backfills entries for existing rows in one
// transaction — the registration never becomes visible without its entries
// (a unique violation in existing data rolls both back).
func (db *DB) CreateIndex(ctx context.Context, table string, def catalog.IndexDef) error {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "create_index", Kind: exec.StmtCreateIndex,
		TableID: resolved.SchemaID, Index: def,
	})
}

// DeleteIndex removes an index; its entries become unreachable garbage.
func (db *DB) DeleteIndex(ctx context.Context, table, index string) error {
	resolved, err := db.table(ctx, table)
	if err != nil {
		return err
	}
	return db.executeCatalog(ctx, exec.ProgramStatement{
		Name: "delete_index", Kind: exec.StmtDeleteIndex,
		TableID: resolved.SchemaID, IndexName: index,
	})
}

func (db *DB) executeCatalog(ctx context.Context, statement exec.ProgramStatement) error {
	_, err := db.eng.ExecuteProgram(ctx, exec.Program{Statements: []exec.ProgramStatement{statement}}, exec.ExecOptions{
		Catalog: exec.CatalogRevisionPerStatement,
	})
	return err
}

func (db *DB) table(ctx context.Context, name string) (catalog.Table, error) {
	table, ok, err := db.cat.GetTable(ctx, name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, reject.Inputf("catalog: table %q does not exist", name)
	}
	return table, nil
}

func (db *DB) tableBySchemaID(ctx context.Context, id catalog.SchemaID) (catalog.Table, error) {
	table, ok, err := db.cat.GetTableBySchemaID(ctx, id)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, reject.Inputf("catalog: table schema ID %d does not exist", id)
	}
	return table, nil
}

func (db *DB) column(ctx context.Context, tableName, columnName string) (catalog.Table, catalog.Column, error) {
	table, err := db.table(ctx, tableName)
	if err != nil {
		return catalog.Table{}, catalog.Column{}, err
	}
	column, ok := table.Column(columnName)
	if !ok {
		return catalog.Table{}, catalog.Column{}, reject.Inputf(
			"catalog: column %q does not exist in table %q", columnName, tableName)
	}
	return table, column, nil
}
