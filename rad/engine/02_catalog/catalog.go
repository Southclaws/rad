// Package catalog is the numbered catalog-layer facade. Catalog values,
// persistence, and mutations live in the model, store, and change packages.
package catalog

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

type Catalog struct {
	store   kv.TransactionalKV
	changes *change.Service
}

func New(database kv.TransactionalKV) *Catalog {
	return &Catalog{store: database, changes: change.New(database)}
}

func (c *Catalog) GetTable(ctx context.Context, name string) (model.Table, bool, error) {
	return c.changes.GetTable(ctx, name)
}

func (c *Catalog) GetTableByID(ctx context.Context, id string) (model.Table, bool, error) {
	return c.changes.GetTableByID(ctx, id)
}

func (c *Catalog) GetTableBySchemaID(ctx context.Context, id model.SchemaID) (model.Table, bool, error) {
	return c.changes.GetTableBySchemaID(ctx, id)
}

func (c *Catalog) ListTables(ctx context.Context) ([]model.Table, error) {
	return c.changes.ListTables(ctx)
}

func (c *Catalog) CreateTable(ctx context.Context, definition model.TableDef) (model.Table, error) {
	return c.changes.CreateTable(ctx, definition)
}

func (c *Catalog) DeleteTable(ctx context.Context, name string) error {
	return c.changes.DeleteTable(ctx, name)
}

func (c *Catalog) RenameTable(ctx context.Context, from, to string) error {
	return c.changes.RenameTable(ctx, from, to)
}

func (c *Catalog) CreateColumn(ctx context.Context, table string, definition model.ColumnDef) (model.Table, error) {
	return c.changes.CreateColumn(ctx, table, definition)
}

func (c *Catalog) DeleteColumn(ctx context.Context, table, column string) (model.Table, error) {
	return c.changes.DeleteColumn(ctx, table, column)
}

func (c *Catalog) RenameColumn(ctx context.Context, table, from, to string) (model.Table, error) {
	return c.changes.RenameColumn(ctx, table, from, to)
}

func (c *Catalog) CreateIndex(ctx context.Context, table string, definition model.IndexDef) (model.Index, error) {
	return c.changes.CreateIndex(ctx, table, definition)
}

func (c *Catalog) DeleteIndex(ctx context.Context, table, index string) error {
	return c.changes.DeleteIndex(ctx, table, index)
}

func (c *Catalog) Mode(ctx context.Context) (model.Mode, error) {
	return c.changes.Mode(ctx)
}

func (c *Catalog) InitMode(ctx context.Context, requested model.Mode) (model.Mode, error) {
	return c.changes.InitMode(ctx, requested)
}

func (c *Catalog) Revision(ctx context.Context) (model.Revision, error) {
	return c.changes.Revision(ctx)
}

func (c *Catalog) Revisions(ctx context.Context) ([]model.Revision, error) {
	return c.changes.Revisions(ctx)
}

func (c *Catalog) Schema(ctx context.Context) (model.Schema, error) {
	return store.New(c.store).Schema(ctx)
}

func (c *Catalog) ValidateCurrentSchema(ctx context.Context) error {
	txn, err := c.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()

	reader := store.New(txn)
	revision, err := reader.Revision(ctx)
	if err != nil {
		return err
	}
	actual, err := reader.Schema(ctx)
	if err != nil {
		return err
	}
	equal, err := revision.Schema.Equal(actual)
	if err != nil {
		return reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: encode schema for drift validation: %w", err))
	}
	if !equal {
		return reject.Fail(reject.ReasonCatalogDrift,
			"catalog: stored schema at version %d does not match physical catalog metadata", revision.Version)
	}
	return nil
}
