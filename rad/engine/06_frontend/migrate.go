package frontend

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	"github.com/Southclaws/rad/rad/engine/06_frontend/migration"
)

func (db *DB) migrationService() *migration.Service { return migration.New(db.eng, db.cat) }

func (db *DB) PlanMigration(ctx context.Context, desired *schema.Schema) (migration.MigrationPlan, error) {
	return db.migrationService().PlanMigration(ctx, desired)
}

func (db *DB) ApplyMigration(ctx context.Context, desired *schema.Schema, acceptDataLoss bool) (migration.MigrationResult, error) {
	return db.migrationService().ApplyMigration(ctx, desired, acceptDataLoss)
}

func (db *DB) ApplyMigrationPlan(ctx context.Context, plan migration.MigrationPlan, acceptDataLoss bool) (migration.MigrationResult, error) {
	return db.migrationService().ApplyMigrationPlan(ctx, plan, acceptDataLoss)
}

func (db *DB) Migrate(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	return db.migrationService().Migrate(ctx, desired)
}

func (db *DB) MigrateFile(ctx context.Context, filename string, src []byte) ([]migrate.Step, error) {
	return db.migrationService().MigrateFile(ctx, filename, src)
}

func (db *DB) PlanMigrationFile(ctx context.Context, filename string, src []byte) (migration.MigrationPlan, error) {
	return db.migrationService().PlanMigrationFile(ctx, filename, src)
}

func (db *DB) ApplyMigrationFile(ctx context.Context, filename string, src []byte, acceptDataLoss bool) (migration.MigrationResult, error) {
	return db.migrationService().ApplyMigrationFile(ctx, filename, src, acceptDataLoss)
}

func (db *DB) Tables(ctx context.Context) ([]model.Table, error) {
	return db.migrationService().Tables(ctx)
}
