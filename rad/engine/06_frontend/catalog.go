package frontend

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"

	"github.com/Southclaws/rad/rad/engine/06_frontend/catalogapi"
)

func (db *DB) catalogService() *catalogapi.Service { return catalogapi.New(db.cat, db.eng) }

func (db *DB) CreateTable(ctx context.Context, def model.TableDef) (model.Table, error) {
	return db.catalogService().CreateTable(ctx, def)
}

func (db *DB) DeleteTable(ctx context.Context, table string) error {
	return db.catalogService().DeleteTable(ctx, table)
}

func (db *DB) RenameTable(ctx context.Context, from, to string) error {
	return db.catalogService().RenameTable(ctx, from, to)
}

func (db *DB) CreateColumn(ctx context.Context, table string, def model.ColumnDef) (model.Table, error) {
	return db.catalogService().CreateColumn(ctx, table, def)
}

func (db *DB) DeleteColumn(ctx context.Context, table, column string) (model.Table, error) {
	return db.catalogService().DeleteColumn(ctx, table, column)
}

func (db *DB) RenameColumn(ctx context.Context, table, from, to string) (model.Table, error) {
	return db.catalogService().RenameColumn(ctx, table, from, to)
}

func (db *DB) CreateIndex(ctx context.Context, table string, def model.IndexDef) error {
	return db.catalogService().CreateIndex(ctx, table, def)
}

func (db *DB) DeleteIndex(ctx context.Context, table, index string) error {
	return db.catalogService().DeleteIndex(ctx, table, index)
}
