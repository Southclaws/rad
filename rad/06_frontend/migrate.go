package frontend

import (
	"context"
	"fmt"

	catalog "rad/rad/02_catalog"
	"rad/rad/02_catalog/migrate"
	"rad/rad/02_catalog/schema"
)

// Migrate reconciles the database with a desired schema: it creates the
// schema namespace if needed, computes the DDL diff, and applies each step
// (catalog mutations, plus index backfills for added indexes). It returns
// the steps it applied — an empty plan means the database already matches.
func (db *DB) Migrate(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	current, err := db.cat.ListTables(ctx)
	if err != nil {
		return nil, err
	}
	steps, err := migrate.Diff(current, desired)
	if err != nil {
		return nil, err
	}

	for _, step := range steps {
		if err := db.applyStep(ctx, step); err != nil {
			return nil, fmt.Errorf("applying %q: %w", step, err)
		}
	}
	return steps, nil
}

func (db *DB) applyStep(ctx context.Context, step migrate.Step) error {
	switch s := step.(type) {
	case migrate.RenameTable:
		return db.cat.RenameTable(ctx, s.From, s.To)
	case migrate.RenameColumn:
		_, err := db.cat.RenameColumn(ctx, s.Table, s.From, s.To)
		return err
	case migrate.CreateTable:
		_, err := db.cat.CreateTable(ctx, s.Def)
		return err
	case migrate.AddColumn:
		_, err := db.cat.AddColumn(ctx, s.Table, s.Def)
		return err
	case migrate.AddIndex:
		// Metadata first, then backfill entries for existing rows. A failed
		// backfill (e.g. duplicate values under a unique index) leaves the
		// index registered but empty; rerunning the migration retries the
		// backfill only after the data is fixed. POC trade-off.
		if _, err := db.cat.AddIndex(ctx, s.Table, s.Def); err != nil {
			return err
		}
		return db.eng.BackfillIndex(ctx, s.Table, s.Def.Name)
	case migrate.DropIndex:
		return db.cat.DropIndex(ctx, s.Table, s.Index)
	case migrate.DropColumn:
		_, err := db.cat.DropColumn(ctx, s.Table, s.Column)
		return err
	case migrate.DropTable:
		return db.cat.DropTable(ctx, s.Table)
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
