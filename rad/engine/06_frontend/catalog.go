package frontend

// The direct catalog-mutation façade. Each call becomes one catalog revision,
// including each reconciler step on a directly managed database. A
// schema-managed reconciler groups the same catalog.Mutation operations in one
// transaction instead, so its complete plan becomes one revision. This layer
// composes operations that need the executor (index backfills) with the ones
// that do not. Catalog-mode enforcement remains a transport concern.

import (
	"context"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

// DeleteTable removes a table from the catalog. Tables referenced by other
// tables' foreign keys must have their referencing tables deleted first.
func (db *DB) DeleteTable(ctx context.Context, table string) error {
	return db.cat.DeleteTable(ctx, table)
}

// RenameTable changes a table's name; metadata-only, data keys are by ID.
func (db *DB) RenameTable(ctx context.Context, from, to string) error {
	return db.cat.RenameTable(ctx, from, to)
}

// CreateColumn appends a column. It must be nullable or carry a literal
// default, since existing rows need a value.
func (db *DB) CreateColumn(ctx context.Context, table string, def catalog.ColumnDef) (catalog.Table, error) {
	return db.cat.CreateColumn(ctx, table, def)
}

// DeleteColumn removes a column not used by the primary key, an index, or a
// foreign key.
func (db *DB) DeleteColumn(ctx context.Context, table, column string) (catalog.Table, error) {
	return db.cat.DeleteColumn(ctx, table, column)
}

// RenameColumn changes a column's name everywhere it appears in metadata.
func (db *DB) RenameColumn(ctx context.Context, table, from, to string) (catalog.Table, error) {
	return db.cat.RenameColumn(ctx, table, from, to)
}

// CreateIndex registers an index and backfills entries for existing rows in one
// transaction — the registration never becomes visible without its entries
// (a unique violation in existing data rolls both back).
func (db *DB) CreateIndex(ctx context.Context, table string, def catalog.IndexDef) error {
	return db.eng.CreateIndexWithBackfill(ctx, table, def)
}

// DeleteIndex removes an index; its entries become unreachable garbage.
func (db *DB) DeleteIndex(ctx context.Context, table, index string) error {
	return db.cat.DeleteIndex(ctx, table, index)
}
