package frontend

import (
	"context"
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
)

// Migrate reconciles the database with a desired schema. Schema-managed
// databases apply the whole plan in one serializable transaction and record
// one revision. Directly managed databases apply each step as its own catalog
// change and therefore record one revision per step. An empty plan records no
// revision in either mode.
func (db *DB) Migrate(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	mode, err := db.cat.Mode(ctx)
	if err != nil {
		return nil, err
	}
	if mode == catalog.ModeSchema {
		return db.migrateSchema(ctx, desired)
	}
	return db.migrateDirect(ctx, desired)
}

func (db *DB) migrateDirect(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	current, err := db.cat.ListTables(ctx)
	if err != nil {
		return nil, err
	}
	steps, err := migrate.Diff(current, desired)
	if err != nil {
		return nil, err
	}
	for _, step := range steps {
		if err := db.applyDirectStep(ctx, step); err != nil {
			return nil, fmt.Errorf("applying %q: %w", step, err)
		}
	}
	return steps, nil
}

func (db *DB) migrateSchema(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	var steps []migrate.Step
	err := db.eng.CatalogTxn(ctx, func(tx *exec.Tx, change *catalog.Mutation) error {
		current, err := change.ListTables(ctx)
		if err != nil {
			return err
		}
		steps, err = migrate.Diff(current, desired)
		if err != nil {
			return err
		}
		for _, step := range steps {
			if err := db.applySchemaStep(ctx, tx, change, step); err != nil {
				return fmt.Errorf("applying %q: %w", step, err)
			}
		}
		return nil
	})
	return steps, err
}

func (db *DB) applyDirectStep(ctx context.Context, step migrate.Step) error {
	switch s := step.(type) {
	case migrate.RenameTable:
		return db.RenameTable(ctx, s.From, s.To)
	case migrate.RenameColumn:
		_, err := db.RenameColumn(ctx, s.Table, s.From, s.To)
		return err
	case migrate.CreateTable:
		_, err := db.CreateTable(ctx, s.Def)
		return err
	case migrate.CreateColumn:
		_, err := db.CreateColumn(ctx, s.Table, s.Def)
		return err
	case migrate.CreateIndex:
		return db.CreateIndex(ctx, s.Table, s.Def)
	case migrate.DeleteIndex:
		return db.DeleteIndex(ctx, s.Table, s.Index)
	case migrate.DeleteColumn:
		_, err := db.DeleteColumn(ctx, s.Table, s.Column)
		return err
	case migrate.DeleteTable:
		return db.DeleteTable(ctx, s.Table)
	default:
		return fmt.Errorf("unknown migration step %T", step)
	}
}

func (db *DB) applySchemaStep(ctx context.Context, tx *exec.Tx, change *catalog.Mutation, step migrate.Step) error {
	switch s := step.(type) {
	case migrate.RenameTable:
		return change.RenameTable(ctx, s.From, s.To)
	case migrate.RenameColumn:
		_, err := change.RenameColumn(ctx, s.Table, s.From, s.To)
		return err
	case migrate.CreateTable:
		_, err := change.CreateTable(ctx, s.Def)
		return err
	case migrate.CreateColumn:
		_, err := change.CreateColumn(ctx, s.Table, s.Def)
		return err
	case migrate.CreateIndex:
		return tx.CreateIndexWithBackfill(ctx, change, s.Table, s.Def)
	case migrate.DeleteIndex:
		return change.DeleteIndex(ctx, s.Table, s.Index)
	case migrate.DeleteColumn:
		_, err := change.DeleteColumn(ctx, s.Table, s.Column)
		return err
	case migrate.DeleteTable:
		return change.DeleteTable(ctx, s.Table)
	default:
		return fmt.Errorf("unknown migration step %T", step)
	}
}

// MigrateFile is Migrate for schema source text (typically a schema.rad
// file's contents).
func (db *DB) MigrateFile(ctx context.Context, filename string, src []byte) ([]migrate.Step, error) {
	desired, err := schema.Parse(filename, src)
	if err != nil {
		return nil, err
	}
	return db.Migrate(ctx, desired)
}

// Tables lists the schema's current table definitions.
func (db *DB) Tables(ctx context.Context) ([]catalog.Table, error) {
	return db.cat.ListTables(ctx)
}
