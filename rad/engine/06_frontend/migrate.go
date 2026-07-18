package frontend

import (
	"context"
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// MigrationResult is the authoritative server outcome of applying a desired
// schema. Revision and its canonical schema are read back only after the
// catalog PIR transaction commits.
type MigrationResult struct {
	Plan     MigrationPlan
	Revision uint64
	Hash     string
	Schema   catalog.Schema
}

// ApplyMigration plans and transactionally applies one desired schema. The
// complete catalog PIR program is one revision in both catalog modes;
// Direct-mode convenience operations remain individual revisions.
func (db *DB) ApplyMigration(ctx context.Context, desired *schema.Schema, acceptDataLoss bool) (MigrationResult, error) {
	plan, err := db.PlanMigration(ctx, desired)
	if err != nil {
		return MigrationResult{}, err
	}
	return db.ApplyMigrationPlan(ctx, plan, acceptDataLoss)
}

// ApplyMigrationPlan commits an exact preflighted transition. The expected
// catalog identity is checked again inside the execution transaction.
func (db *DB) ApplyMigrationPlan(ctx context.Context, plan MigrationPlan, acceptDataLoss bool) (MigrationResult, error) {
	if len(plan.Blocking) > 0 {
		return MigrationResult{}, reject.Fail(reject.ReasonConstraintViolation,
			"migration target is invalid: %s", plan.Blocking[0].Summary)
	}
	if len(plan.Destructive) > 0 && !acceptDataLoss {
		return MigrationResult{}, reject.Fail(reject.ReasonDataLossAcceptance,
			"migration will delete data: %s", plan.Destructive[0].Summary)
	}
	if len(plan.Steps) > 0 {
		expected := plan.Current
		if _, err := db.eng.ExecuteProgram(ctx, plan.Program, exec.ExecOptions{
			Catalog: exec.CatalogRevisionPerProgram, ExpectedCatalog: &expected,
		}); err != nil {
			return MigrationResult{}, err
		}
	}
	revision, err := db.cat.Revision(ctx)
	if err != nil {
		return MigrationResult{}, err
	}
	if len(plan.Steps) > 0 && revision.Version != plan.Current.Version+1 {
		return MigrationResult{}, fmt.Errorf(
			"migration: committed schema version %d, want %d", revision.Version, plan.Current.Version+1)
	}
	return MigrationResult{
		Plan: plan, Revision: revision.Version, Hash: revision.Hash, Schema: revision.Schema,
	}, nil
}

// Migrate reconciles the database with a desired schema without granting
// data-loss consent. Call ApplyMigration when the caller has explicitly
// accepted destructive findings.
func (db *DB) Migrate(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	result, err := db.ApplyMigration(ctx, desired, false)
	return result.Plan.Steps, err
}

func (db *DB) MigrateFile(ctx context.Context, filename string, src []byte) ([]migrate.Step, error) {
	desired, err := schema.Parse(filename, src)
	if err != nil {
		return nil, err
	}
	return db.Migrate(ctx, desired)
}

func (db *DB) PlanMigrationFile(ctx context.Context, filename string, src []byte) (MigrationPlan, error) {
	desired, err := schema.Parse(filename, src)
	if err != nil {
		return MigrationPlan{}, err
	}
	return db.PlanMigration(ctx, desired)
}

func (db *DB) ApplyMigrationFile(
	ctx context.Context,
	filename string,
	src []byte,
	acceptDataLoss bool,
) (MigrationResult, error) {
	desired, err := schema.Parse(filename, src)
	if err != nil {
		return MigrationResult{}, err
	}
	return db.ApplyMigration(ctx, desired, acceptDataLoss)
}

// Tables lists the schema's current table definitions.
func (db *DB) Tables(ctx context.Context) ([]catalog.Table, error) {
	return db.cat.ListTables(ctx)
}
