package frontend

// The catalog-mutation façade: one set of schema-change entry points shared
// by every channel — the schema.rad reconciler (migrate.go) and the
// transport's imperative catalog endpoints both land here. The catalog
// defines the semantics; this layer only composes the operations that need
// the executor (index backfills) with the ones that don't. Catalog-mode
// enforcement is a transport concern: both channels call the same methods,
// so the distinction between direct and schema-managed mutation exists only
// at the endpoint.

import (
	"context"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

// DropTable removes a table from the catalog. Tables referenced by other
// tables' foreign keys must have their referencing tables dropped first.
func (db *DB) DropTable(ctx context.Context, table string) error {
	return db.cat.DropTable(ctx, table)
}

// RenameTable changes a table's name; metadata-only, data keys are by ID.
func (db *DB) RenameTable(ctx context.Context, from, to string) error {
	return db.cat.RenameTable(ctx, from, to)
}

// AddColumn appends a column. It must be nullable or carry a literal
// default, since existing rows need a value.
func (db *DB) AddColumn(ctx context.Context, table string, def catalog.ColumnDef) (catalog.Table, error) {
	return db.cat.AddColumn(ctx, table, def)
}

// DropColumn removes a column not used by the primary key, an index, or a
// foreign key.
func (db *DB) DropColumn(ctx context.Context, table, column string) (catalog.Table, error) {
	return db.cat.DropColumn(ctx, table, column)
}

// RenameColumn changes a column's name everywhere it appears in metadata.
func (db *DB) RenameColumn(ctx context.Context, table, from, to string) (catalog.Table, error) {
	return db.cat.RenameColumn(ctx, table, from, to)
}

// AddIndex registers an index and backfills entries for existing rows in one
// transaction — the registration never becomes visible without its entries
// (a unique violation in existing data rolls both back).
func (db *DB) AddIndex(ctx context.Context, table string, def catalog.IndexDef) error {
	return db.eng.AddIndexWithBackfill(ctx, table, def)
}

// DropIndex removes an index; its entries become unreachable garbage.
func (db *DB) DropIndex(ctx context.Context, table, index string) error {
	return db.cat.DropIndex(ctx, table, index)
}
