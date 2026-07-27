package migration

import (
	"context"
	"fmt"
	"time"

	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// MigrationResult is the authoritative server outcome of applying a desired
// schema. Revision and its canonical schema are read back only after the
// catalog PIR transaction commits.
type MigrationResult struct {
	Plan          MigrationPlan
	Revision      uint64
	Hash          string
	Schema        model.Schema
	State         MigrationState
	TransitionIDs []string
}

type MigrationState string

const (
	MigrationConverging MigrationState = "converging"
	MigrationReady      MigrationState = "ready"
)

// WaitMigration observes the durable work returned by ApplyMigration and
// waits until publication reaches the desired canonical hash. It never holds
// the apply transaction open; workers continue in bounded independent
// transactions and this method can be cancelled safely.
func (db *Service) WaitMigration(ctx context.Context, result MigrationResult) (MigrationResult, error) {
	if result.State == MigrationReady {
		return result, nil
	}
	if result.State != MigrationConverging || len(result.TransitionIDs) == 0 {
		return MigrationResult{}, fmt.Errorf(
			"migration: state %q has no observable convergence work",
			result.State,
		)
	}
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	for {
		allReady := true
		for _, transitionID := range result.TransitionIDs {
			transition, ok, err := db.catalog.GetTransition(ctx, transitionID)
			if err != nil {
				return MigrationResult{}, err
			}
			if !ok {
				return MigrationResult{}, fmt.Errorf(
					"migration: transition %q no longer exists",
					transitionID,
				)
			}
			switch transition.State {
			case model.TransitionReady:
			case model.TransitionFailed, model.TransitionCancelled:
				return MigrationResult{}, fmt.Errorf(
					"migration: transition %q ended in state %q: %s",
					transitionID,
					transition.State,
					transition.LastError,
				)
			default:
				allReady = false
			}
		}
		if allReady {
			revision, err := db.catalog.Revision(ctx)
			if err != nil {
				return MigrationResult{}, err
			}
			if revision.Hash != result.Plan.DesiredHash {
				return MigrationResult{}, fmt.Errorf(
					"migration: transitions published but current hash is %s, want %s",
					revision.Hash,
					result.Plan.DesiredHash,
				)
			}
			result.Revision = revision.Version
			result.Hash = revision.Hash
			result.Schema = revision.Schema
			result.State = MigrationReady
			return result, nil
		}
		select {
		case <-ctx.Done():
			return MigrationResult{}, ctx.Err()
		case <-ticker.C:
		}
	}
}

// ApplyMigration plans and transactionally applies one desired schema. The
// complete catalog PIR program is one revision in both catalog modes;
// Direct-mode convenience operations remain individual revisions.
func (db *Service) ApplyMigration(ctx context.Context, desired *schema.Schema, acceptDataLoss bool) (MigrationResult, error) {
	plan, err := db.PlanMigration(ctx, desired)
	if err != nil {
		return MigrationResult{}, err
	}
	return db.ApplyMigrationPlan(ctx, plan, acceptDataLoss)
}

// ApplyMigrationPlan commits an exact preflighted transition. The expected
// catalog identity is checked again inside the execution transaction.
func (db *Service) ApplyMigrationPlan(ctx context.Context, plan MigrationPlan, acceptDataLoss bool) (MigrationResult, error) {
	if len(plan.Blocking) > 0 {
		return MigrationResult{}, reject.Fail(reject.ReasonConstraintViolation,
			"migration target is invalid: %s", plan.Blocking[0].Summary)
	}
	if len(plan.Destructive) > 0 && !acceptDataLoss {
		return MigrationResult{}, reject.Fail(reject.ReasonDataLossAcceptance,
			"migration will delete data: %s", plan.Destructive[0].Summary)
	}
	controls := append([]model.TransitionControl(nil), plan.Transitions...)
	if len(plan.Program.Statements) > 0 {
		expected := plan.Current
		executed, err := db.engine.ExecuteProgram(ctx, plan.Program, execprogram.Options{
			Catalog: execprogram.CatalogRevisionPerProgram, ExpectedCatalog: &expected,
		})
		if err != nil {
			return MigrationResult{}, err
		}
		for _, statement := range executed.Statements {
			if statement.Control != nil {
				controls = append(controls, *statement.Control)
			}
		}
	}
	revision, err := db.catalog.Revision(ctx)
	if err != nil {
		return MigrationResult{}, err
	}
	state := MigrationReady
	if revision.Hash != plan.DesiredHash {
		if len(controls) == 0 {
			return MigrationResult{}, fmt.Errorf(
				"migration: catalog reached hash %s without durable work toward desired hash %s",
				revision.Hash,
				plan.DesiredHash,
			)
		}
		state = MigrationConverging
	}
	transitionIDs := make([]string, len(controls))
	for i, control := range controls {
		transitionIDs[i] = control.TransitionID
	}
	return MigrationResult{
		Plan: plan, Revision: revision.Version, Hash: revision.Hash, Schema: revision.Schema,
		State: state, TransitionIDs: transitionIDs,
	}, nil
}

// Migrate reconciles the database with a desired schema without granting
// data-loss consent. Call ApplyMigration when the caller has explicitly
// accepted destructive findings.
func (db *Service) Migrate(ctx context.Context, desired *schema.Schema) ([]migrate.Step, error) {
	result, err := db.ApplyMigration(ctx, desired, false)
	steps := result.Plan.Steps
	if err == nil {
		_, err = db.WaitMigration(ctx, result)
	}
	return steps, err
}

func (db *Service) MigrateFile(ctx context.Context, filename string, src []byte) ([]migrate.Step, error) {
	desired, err := schema.Parse(filename, src)
	if err != nil {
		return nil, err
	}
	return db.Migrate(ctx, desired)
}

func (db *Service) PlanMigrationFile(ctx context.Context, filename string, src []byte) (MigrationPlan, error) {
	desired, err := schema.Parse(filename, src)
	if err != nil {
		return MigrationPlan{}, err
	}
	return db.PlanMigration(ctx, desired)
}

func (db *Service) ApplyMigrationFile(
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
func (db *Service) Tables(ctx context.Context) ([]model.Table, error) {
	return db.catalog.ListTables(ctx)
}
