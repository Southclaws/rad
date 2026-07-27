package exec

import (
	"context"
	"fmt"
	"testing"
	"time"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

func TestWaitingReplacementChainRecoversAndActivatesInDependencyOrder(t *testing.T) {
	ctx := context.Background()
	databaseName := t.TempDir()
	firstStore, first := openRecoveryEngine(t, databaseName)
	table, err := first.Catalog().CreateTable(ctx, model.TableDef{
		Name: "recover_waiting_chain",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	for rowID := range 5 {
		if err := first.Insert(ctx, table.Name, lir.Row{
			"id": lir.Int64(int64(rowID)), "value": lir.Int64(int64(rowID * 7)),
		}); err != nil {
			t.Fatal(err)
		}
	}
	source, _ := table.Column("value")
	firstTransition, err := first.startColumnReplacement(
		ctx,
		table.SchemaID,
		source.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	dependent, err := first.startColumnReplacement(
		ctx,
		table.SchemaID,
		source.SchemaID,
		model.ColumnReplacementDef{
			Type: model.TypeInt64, Nullable: true,
			Prerequisites: []string{firstTransition.ID},
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	if dependent.State != model.TransitionWaiting ||
		dependent.ReplacementRequest == nil ||
		dependent.ColumnReplacement != nil {
		t.Fatalf("durable waiting replacement = %+v", dependent)
	}
	retainTransitionDiagnostics(t, ctx, first, firstTransition.ID)
	retainTransitionDiagnostics(t, ctx, first, dependent.ID)
	closeRecoveryEngine(t, first, firstStore)

	_, reopened := reopenRecoveryEngine(t, databaseName)
	firstReady := waitForTransitionState(
		t,
		ctx,
		reopened,
		firstTransition.ID,
		model.TransitionReady,
	)
	dependentReady := waitForTransitionState(
		t,
		ctx,
		reopened,
		dependent.ID,
		model.TransitionReady,
	)
	if firstReady.ColumnReplacement == nil ||
		dependentReady.ColumnReplacement == nil ||
		dependentReady.ColumnReplacement.Source.ID !=
			firstReady.ColumnReplacement.Target.ID {
		t.Fatalf(
			"recovered dependency order: first=%+v dependent=%+v",
			firstReady,
			dependentReady,
		)
	}
	for rowID := range 5 {
		row, found, err := reopened.GetByPrimaryKey(
			ctx,
			table.Name,
			lir.Row{"id": lir.Int64(int64(rowID))},
		)
		if err != nil || !found ||
			!row["value"].Equal(lir.Int64(int64(rowID*7))) {
			t.Fatalf("recovered chained row %d = %v found=%v err=%v", rowID, row, found, err)
		}
	}
}

func TestColumnReplacementRecoversFromEveryDurableCheckpoint(t *testing.T) {
	checkpoints := []struct {
		name    string
		advance func(*testing.T, context.Context, *Engine, string) model.SchemaTransition
	}{
		{
			name: "write-protocol-published",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				transition, err := engine.inspectSchemaTransition(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				return transition
			},
		},
		{
			name: "partial-backfill-checkpoint",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				owner, err := engine.claimColumnReplacement(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				transition, err := engine.stepColumnReplacement(ctx, id, owner, 2)
				if err != nil {
					t.Fatal(err)
				}
				if transition.RowsScanned != 2 || len(transition.Cursor) == 0 {
					t.Fatalf("partial replacement checkpoint = %+v", transition)
				}
				return transition
			},
		},
		{
			name: "finalization-gate-published",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				owner, err := engine.claimColumnReplacement(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				for range 32 {
					transition, err := engine.stepColumnReplacement(ctx, id, owner, 2)
					if err != nil {
						t.Fatal(err)
					}
					if transition.State == model.TransitionValidating {
						return transition
					}
				}
				t.Fatalf("replacement %q did not reach its finalization gate", id)
				return model.SchemaTransition{}
			},
		},
	}

	for _, checkpoint := range checkpoints {
		t.Run(checkpoint.name, func(t *testing.T) {
			ctx := context.Background()
			databaseName := t.TempDir()
			firstStore, first := openRecoveryEngine(t, databaseName)
			table, err := first.Catalog().CreateTable(ctx, model.TableDef{
				Name: "recover_replacement",
				Columns: []model.ColumnDef{
					{Name: "id", Type: model.TypeInt64},
					{Name: "value", Type: model.TypeInt64, Nullable: true},
				},
				PrimaryKey: []string{"id"},
			})
			if err != nil {
				t.Fatal(err)
			}
			for rowID := range 7 {
				if err := first.Insert(ctx, table.Name, lir.Row{
					"id": lir.Int64(int64(rowID)), "value": lir.Int64(int64(rowID * 11)),
				}); err != nil {
					t.Fatal(err)
				}
			}
			source, ok := table.Column("value")
			if !ok {
				t.Fatal("replacement source column is missing")
			}
			started, err := first.startColumnReplacement(
				ctx,
				table.SchemaID,
				source.SchemaID,
				model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
			)
			if err != nil {
				t.Fatal(err)
			}
			before := checkpoint.advance(t, ctx, first, started.ID)
			retainTransitionDiagnostics(t, ctx, first, started.ID)
			closeRecoveryEngine(t, first, firstStore)

			_, reopened := reopenRecoveryEngine(t, databaseName)
			ready := waitForTransitionState(t, ctx, reopened, started.ID, model.TransitionReady)
			if ready.OwnerEpoch <= before.OwnerEpoch || ready.ColumnReplacement == nil {
				t.Fatalf("recovered replacement = %+v, checkpoint=%+v", ready, before)
			}
			reclamationID := store.ReplacedColumnReclamationID(started.ID)
			waitForReclamationState(
				t,
				ctx,
				reopened.store,
				reclamationID,
				model.ReclamationReclaimed,
			)
			assertRecoveredReplacementRows(t, ctx, reopened, table, source, 7)
			assertDetailedTransitionRetained(t, ctx, reopened, started.ID)
			if err := reopened.releaseRetention(ctx, recoveryDiagnosticPinID(started.ID)); err != nil {
				t.Fatal(err)
			}
			waitForTransitionCompaction(t, ctx, reopened, started.ID, model.TransitionReady)
		})
	}
}

func TestConstraintValidationRecoversFromEveryDurableCheckpoint(t *testing.T) {
	checkpoints := []struct {
		name    string
		advance func(*testing.T, context.Context, *Engine, string) model.SchemaTransition
	}{
		{
			name: "foreground-enforcement-published",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				transition, err := engine.inspectSchemaTransition(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				return transition
			},
		},
		{
			name: "historical-validation-published",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				owner, err := engine.claimConstraintValidation(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				transition, err := engine.stepConstraintValidation(ctx, id, owner, 2)
				if err != nil {
					t.Fatal(err)
				}
				if transition.Constraint == nil ||
					transition.Constraint.State != model.ConstraintValidatingExisting {
					t.Fatalf("historical validation checkpoint = %+v", transition)
				}
				return transition
			},
		},
		{
			name: "partial-validation-checkpoint",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				owner, err := engine.claimConstraintValidation(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				if _, err := engine.stepConstraintValidation(ctx, id, owner, 2); err != nil {
					t.Fatal(err)
				}
				transition, err := engine.stepConstraintValidation(ctx, id, owner, 2)
				if err != nil {
					t.Fatal(err)
				}
				if transition.RowsScanned != 2 || len(transition.Cursor) == 0 {
					t.Fatalf("partial constraint checkpoint = %+v", transition)
				}
				return transition
			},
		},
		{
			name: "finalization-gate-published",
			advance: func(t *testing.T, ctx context.Context, engine *Engine, id string) model.SchemaTransition {
				t.Helper()
				owner, err := engine.claimConstraintValidation(ctx, id)
				if err != nil {
					t.Fatal(err)
				}
				for range 32 {
					transition, err := engine.stepConstraintValidation(ctx, id, owner, 2)
					if err != nil {
						t.Fatal(err)
					}
					if transition.State == model.TransitionValidating {
						return transition
					}
				}
				t.Fatalf("constraint %q did not reach its finalization gate", id)
				return model.SchemaTransition{}
			},
		},
	}

	for _, checkpoint := range checkpoints {
		t.Run(checkpoint.name, func(t *testing.T) {
			ctx := context.Background()
			databaseName := t.TempDir()
			firstStore, first := openRecoveryEngine(t, databaseName)
			table, err := first.Catalog().CreateTable(ctx, model.TableDef{
				Name: "recover_constraint",
				Columns: []model.ColumnDef{
					{Name: "id", Type: model.TypeInt64},
					{Name: "value", Type: model.TypeText, Nullable: true},
				},
				PrimaryKey: []string{"id"},
			})
			if err != nil {
				t.Fatal(err)
			}
			for rowID := range 7 {
				if err := first.Insert(ctx, table.Name, lir.Row{
					"id":    lir.Int64(int64(rowID)),
					"value": lir.Text(fmt.Sprintf("value-%d", rowID)),
				}); err != nil {
					t.Fatal(err)
				}
			}
			column, ok := table.Column("value")
			if !ok {
				t.Fatal("constraint column is missing")
			}
			started, err := first.startConstraintValidation(
				ctx,
				table.SchemaID,
				model.ConstraintDef{
					Name: "recover_value_required",
					Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
				},
			)
			if err != nil {
				t.Fatal(err)
			}
			before := checkpoint.advance(t, ctx, first, started.ID)
			retainTransitionDiagnostics(t, ctx, first, started.ID)
			closeRecoveryEngine(t, first, firstStore)

			_, reopened := reopenRecoveryEngine(t, databaseName)
			ready := waitForTransitionState(t, ctx, reopened, started.ID, model.TransitionReady)
			if ready.OwnerEpoch <= before.OwnerEpoch || ready.Constraint == nil ||
				ready.Constraint.State != model.ConstraintValid {
				t.Fatalf("recovered constraint = %+v, checkpoint=%+v", ready, before)
			}
			reclamationID := store.ConstraintValidationReclamationID(started.ID)
			waitForReclamationState(
				t,
				ctx,
				reopened.store,
				reclamationID,
				model.ReclamationReclaimed,
			)
			start, end := store.TransitionViolationRange(started.ID)
			assertRangeCount(t, ctx, reopened.store, start, end, 0)
			published, ok, err := reopened.Catalog().GetTable(ctx, table.Name)
			if err != nil || !ok {
				t.Fatalf("recovered constraint table: found=%v err=%v", ok, err)
			}
			publishedColumn, ok := published.Column("value")
			if !ok || publishedColumn.Nullable {
				t.Fatalf("recovered constraint column = %+v found=%v", publishedColumn, ok)
			}
			assertDetailedTransitionRetained(t, ctx, reopened, started.ID)
			if err := reopened.releaseRetention(ctx, recoveryDiagnosticPinID(started.ID)); err != nil {
				t.Fatal(err)
			}
			waitForTransitionCompaction(t, ctx, reopened, started.ID, model.TransitionReady)
		})
	}
}

func TestFailedSlice8TransitionsRecoverAndReclaim(t *testing.T) {
	t.Run("replacement conversion failure", func(t *testing.T) {
		ctx := context.Background()
		databaseName := t.TempDir()
		firstStore, first := openRecoveryEngine(t, databaseName)
		table, err := first.Catalog().CreateTable(ctx, model.TableDef{
			Name: "recover_failed_replacement",
			Columns: []model.ColumnDef{
				{Name: "id", Type: model.TypeInt64},
				{Name: "value", Type: model.TypeText, Nullable: true},
			},
			PrimaryKey: []string{"id"},
		})
		if err != nil {
			t.Fatal(err)
		}
		for rowID, value := range []string{"42", "not-an-int"} {
			if err := first.Insert(ctx, table.Name, lir.Row{
				"id": lir.Int64(int64(rowID)), "value": lir.Text(value),
			}); err != nil {
				t.Fatal(err)
			}
		}
		source, ok := table.Column("value")
		if !ok {
			t.Fatal("failed replacement source is missing")
		}
		started, err := first.startColumnReplacement(
			ctx,
			table.SchemaID,
			source.SchemaID,
			model.ColumnReplacementDef{Type: model.TypeInt64, Nullable: true},
		)
		if err != nil {
			t.Fatal(err)
		}
		owner, err := first.claimColumnReplacement(ctx, started.ID)
		if err != nil {
			t.Fatal(err)
		}
		checkpoint, err := first.stepColumnReplacement(ctx, started.ID, owner, 8)
		if err != nil {
			t.Fatal(err)
		}
		if checkpoint.RowsScanned != 2 {
			t.Fatalf("failed replacement checkpoint = %+v", checkpoint)
		}
		if _, _, violation, err := store.FirstTransitionViolation(
			ctx,
			first.store,
			started.ID,
		); err != nil || !violation {
			t.Fatalf("conversion failure marker: found=%v err=%v", violation, err)
		}
		retainTransitionDiagnostics(t, ctx, first, started.ID)
		closeRecoveryEngine(t, first, firstStore)

		_, reopened := reopenRecoveryEngine(t, databaseName)
		failed := waitForTransitionState(
			t,
			ctx,
			reopened,
			started.ID,
			model.TransitionFailed,
		)
		if failed.LastError == "" || failed.ColumnReplacement == nil {
			t.Fatalf("recovered failed replacement = %+v", failed)
		}
		waitForReclamationState(
			t,
			ctx,
			reopened.store,
			store.FailedReplacementReclamationID(started.ID),
			model.ReclamationReclaimed,
		)
		published, active := replacementColumn(
			t,
			ctx,
			reopened,
			table.Name,
			"value",
		)
		if active.ID != source.ID || active.Type != model.TypeText {
			t.Fatalf("failed replacement changed active representation: %+v", active)
		}
		assertPhysicalColumnAbsent(
			t,
			ctx,
			reopened,
			published,
			started.ColumnReplacement.Target.ID,
			2,
		)
		start, end := store.TransitionViolationRange(started.ID)
		assertRangeCount(t, ctx, reopened.store, start, end, 0)
		assertDetailedTransitionRetained(t, ctx, reopened, started.ID)
		if err := reopened.releaseRetention(ctx, recoveryDiagnosticPinID(started.ID)); err != nil {
			t.Fatal(err)
		}
		waitForTransitionCompaction(t, ctx, reopened, started.ID, model.TransitionFailed)
	})

	t.Run("constraint validation failure", func(t *testing.T) {
		ctx := context.Background()
		databaseName := t.TempDir()
		firstStore, first := openRecoveryEngine(t, databaseName)
		table, err := first.Catalog().CreateTable(ctx, model.TableDef{
			Name: "recover_failed_constraint",
			Columns: []model.ColumnDef{
				{Name: "id", Type: model.TypeInt64},
				{Name: "value", Type: model.TypeText, Nullable: true},
			},
			PrimaryKey: []string{"id"},
		})
		if err != nil {
			t.Fatal(err)
		}
		if err := first.Insert(ctx, table.Name, lir.Row{"id": lir.Int64(1)}); err != nil {
			t.Fatal(err)
		}
		column, ok := table.Column("value")
		if !ok {
			t.Fatal("failed constraint column is missing")
		}
		started, err := first.startConstraintValidation(
			ctx,
			table.SchemaID,
			model.ConstraintDef{
				Name: "recover_failed_value_required",
				Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
			},
		)
		if err != nil {
			t.Fatal(err)
		}
		owner, err := first.claimConstraintValidation(ctx, started.ID)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := first.stepConstraintValidation(ctx, started.ID, owner, 8); err != nil {
			t.Fatal(err)
		}
		checkpoint, err := first.stepConstraintValidation(ctx, started.ID, owner, 8)
		if err != nil {
			t.Fatal(err)
		}
		if checkpoint.RowsScanned != 1 {
			t.Fatalf("failed constraint checkpoint = %+v", checkpoint)
		}
		if _, _, violation, err := store.FirstTransitionViolation(
			ctx,
			first.store,
			started.ID,
		); err != nil || !violation {
			t.Fatalf("constraint failure marker: found=%v err=%v", violation, err)
		}
		retainTransitionDiagnostics(t, ctx, first, started.ID)
		closeRecoveryEngine(t, first, firstStore)

		_, reopened := reopenRecoveryEngine(t, databaseName)
		failed := waitForTransitionState(
			t,
			ctx,
			reopened,
			started.ID,
			model.TransitionFailed,
		)
		if failed.LastError == "" || failed.Constraint == nil ||
			failed.Constraint.State != model.ConstraintFailed {
			t.Fatalf("recovered failed constraint = %+v", failed)
		}
		waitForReclamationState(
			t,
			ctx,
			reopened.store,
			store.ConstraintValidationReclamationID(started.ID),
			model.ReclamationReclaimed,
		)
		start, end := store.TransitionViolationRange(started.ID)
		assertRangeCount(t, ctx, reopened.store, start, end, 0)
		published, ok, err := reopened.Catalog().GetTable(ctx, table.Name)
		if err != nil || !ok {
			t.Fatalf("failed constraint table: found=%v err=%v", ok, err)
		}
		active, ok := published.Column("value")
		if !ok || !active.Nullable {
			t.Fatalf("failed constraint changed canonical nullability: %+v", active)
		}
		assertDetailedTransitionRetained(t, ctx, reopened, started.ID)
		if err := reopened.releaseRetention(ctx, recoveryDiagnosticPinID(started.ID)); err != nil {
			t.Fatal(err)
		}
		waitForTransitionCompaction(t, ctx, reopened, started.ID, model.TransitionFailed)
	})
}

func openRecoveryEngine(
	t *testing.T,
	databaseName string,
) (*kvslate.Store, *Engine) {
	t.Helper()
	return openFileEngine(
		t,
		databaseName,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
}

func closeRecoveryEngine(t *testing.T, engine *Engine, database *kvslate.Store) {
	t.Helper()
	if err := engine.Close(); err != nil {
		t.Fatal(err)
	}
	if err := database.Close(); err != nil {
		t.Fatal(err)
	}
}

func reopenRecoveryEngine(
	t *testing.T,
	databaseName string,
) (*kvslate.Store, *Engine) {
	t.Helper()
	return openFileEngine(t, databaseName, WithSchemaJobConfig(recoverySchemaJobConfig()))
}

func recoverySchemaJobConfig() SchemaJobConfig {
	return SchemaJobConfig{
		IndexBatchSize: 2, ReclamationBatchSize: 2,
		BatchesBeforeYield: 1, IOBudgetItemsPerYield: 2,
		YieldInterval: time.Millisecond,
	}
}

func retainTransitionDiagnostics(
	t *testing.T,
	ctx context.Context,
	engine *Engine,
	transitionID string,
) {
	t.Helper()
	if err := engine.retain(ctx, model.RetentionPin{
		ID:        recoveryDiagnosticPinID(transitionID),
		OwnerKind: model.RetentionOwnerSchemaWorker,
		OwnerID:   "recovery-test",
		Resource: model.RetentionResource{
			Kind: model.RetentionTransitionDiagnostics, TransitionID: transitionID,
		},
	}); err != nil {
		t.Fatal(err)
	}
}

func recoveryDiagnosticPinID(transitionID string) string {
	return "recovery-diagnostics-" + transitionID
}

func assertRecoveredReplacementRows(
	t *testing.T,
	ctx context.Context,
	engine *Engine,
	table model.Table,
	source model.Column,
	count int,
) {
	t.Helper()
	for rowID := range count {
		key := lir.Row{"id": lir.Int64(int64(rowID))}
		row, found, err := engine.GetByPrimaryKey(ctx, table.Name, key)
		if err != nil || !found || row["value"] != lir.Text(fmt.Sprintf("%d", rowID*11)) {
			t.Fatalf("recovered row %d = %v found=%v err=%v", rowID, row, found, err)
		}
		pk, err := codec.EncodeRowTuple(key, table.PrimaryKey)
		if err != nil {
			t.Fatal(err)
		}
		raw, found, err := engine.store.Get(ctx, codec.DataKey(table.ID, pk))
		if err != nil || !found {
			t.Fatalf("raw recovered row %d: found=%v err=%v", rowID, found, err)
		}
		if _, sourceFound, err := codec.RemoveColumn(raw, source.ID); err != nil || sourceFound {
			t.Fatalf("retired source cell on row %d: found=%v err=%v", rowID, sourceFound, err)
		}
	}
}

func assertDetailedTransitionRetained(
	t *testing.T,
	ctx context.Context,
	engine *Engine,
	transitionID string,
) {
	t.Helper()
	transition, err := engine.inspectSchemaTransition(ctx, transitionID)
	if err != nil {
		t.Fatal(err)
	}
	if !transition.CompactedAt.IsZero() ||
		(transition.ColumnReplacement == nil && transition.Constraint == nil) {
		t.Fatalf("diagnostic pin did not retain detailed transition: %+v", transition)
	}
}

func assertPhysicalColumnAbsent(
	t *testing.T,
	ctx context.Context,
	engine *Engine,
	table model.Table,
	columnID string,
	count int,
) {
	t.Helper()
	for rowID := range count {
		key := lir.Row{"id": lir.Int64(int64(rowID))}
		pk, err := codec.EncodeRowTuple(key, table.PrimaryKey)
		if err != nil {
			t.Fatal(err)
		}
		raw, found, err := engine.store.Get(ctx, codec.DataKey(table.ID, pk))
		if err != nil || !found {
			t.Fatalf("raw row %d: found=%v err=%v", rowID, found, err)
		}
		if _, columnFound, err := codec.RemoveColumn(raw, columnID); err != nil || columnFound {
			t.Fatalf("retired physical column on row %d: found=%v err=%v", rowID, columnFound, err)
		}
	}
}

func waitForTransitionCompaction(
	t *testing.T,
	ctx context.Context,
	engine *Engine,
	transitionID string,
	wantState model.TransitionState,
) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for {
		transition, err := engine.inspectSchemaTransition(ctx, transitionID)
		if err != nil {
			t.Fatal(err)
		}
		if !transition.CompactedAt.IsZero() {
			if transition.State != wantState ||
				transition.ColumnReplacement != nil ||
				transition.Constraint != nil {
				t.Fatalf("compacted transition retained worker state: %+v", transition)
			}
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("transition %q did not compact after diagnostic release: %+v", transitionID, transition)
		}
		time.Sleep(time.Millisecond)
	}
}
