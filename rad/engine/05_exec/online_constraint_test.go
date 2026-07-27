package exec

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestConstraintLifecycleEnforcementValidationAndPublication(t *testing.T) {
	eng, ctx := setupConstraintTable(t)
	table, column := replacementColumn(t, ctx, eng, "readings", "value")
	transition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
		Name: "readings_value_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
	})
	if err != nil {
		t.Fatal(err)
	}
	if transition.State != model.TransitionBuilding ||
		transition.Constraint.State != model.ConstraintEnforcingNewWrites {
		t.Fatalf("started constraint = %+v", transition)
	}
	if err := eng.Insert(ctx, "readings", lir.Row{"id": lir.Int64(41)}); err == nil ||
		!strings.Contains(err.Error(), "rejects NULL") {
		t.Fatalf("active constraint accepted NULL: %v", err)
	}
	if err := eng.Insert(ctx, "readings", lir.Row{
		"id": lir.Int64(41), "value": lir.Text("during-validation"),
	}); err != nil {
		t.Fatal(err)
	}

	owner, err := eng.claimConstraintValidation(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	scanning, err := eng.stepConstraintValidation(ctx, transition.ID, owner, 2)
	if err != nil {
		t.Fatal(err)
	}
	if scanning.Constraint.State != model.ConstraintValidatingExisting ||
		scanning.State != model.TransitionBuilding {
		t.Fatalf("historical validation state = %+v", scanning)
	}
	ready := driveConstraintValidation(t, ctx, eng, transition.ID, owner, 2)
	if ready.State != model.TransitionReady ||
		ready.Constraint.State != model.ConstraintValid ||
		ready.RowsScanned != 7 {
		t.Fatalf("valid constraint = %+v", ready)
	}
	published, publishedColumn := replacementColumn(t, ctx, eng, "readings", "value")
	if publishedColumn.Nullable {
		t.Fatalf("valid not-null constraint did not update canonical column: %+v", publishedColumn)
	}
	publishedConstraint, ok := tableConstraint(published, transition.Constraint.ID)
	if !ok || publishedConstraint.State != model.ConstraintValid {
		t.Fatalf("published constraint = %+v ok=%v", publishedConstraint, ok)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, published)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ConstraintChecks) != 0 || protocol.FinalizationGate != nil {
		t.Fatalf("valid constraint retained transition protocol: %+v", protocol)
	}
	if err := eng.Insert(ctx, "readings", lir.Row{"id": lir.Int64(42)}); err == nil ||
		!strings.Contains(err.Error(), "not nullable") {
		t.Fatalf("canonical not-null column accepted NULL: %v", err)
	}
}

func TestConstraintEnforcementFencesWriterAdmittedBeforeActivation(t *testing.T) {
	eng, ctx := setupConstraintTable(t)
	writer, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer writer.Rollback()
	if err := writer.Insert(ctx, "readings", lir.Row{"id": lir.Int64(99)}); err != nil {
		t.Fatal(err)
	}
	table, column := replacementColumn(t, ctx, eng, "readings", "value")
	if _, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
		Name: "readings_value_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
	}); err != nil {
		t.Fatal(err)
	}
	if err := writer.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("pre-enforcement NULL writer commit = %v, want conflict", err)
	}
	if _, ok, err := eng.GetByPrimaryKey(ctx, "readings", lir.Row{"id": lir.Int64(99)}); err != nil || ok {
		t.Fatalf("conflicted NULL row visible: ok=%v err=%v", ok, err)
	}
}

func TestConstraintValidationRepairClearsDurableViolation(t *testing.T) {
	eng, ctx := setupConstraintTable(t)
	if err := eng.Insert(ctx, "readings", lir.Row{"id": lir.Int64(20)}); err != nil {
		t.Fatal(err)
	}
	table, column := replacementColumn(t, ctx, eng, "readings", "value")
	transition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
		Name: "readings_value_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
	})
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimConstraintValidation(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepConstraintValidation(ctx, transition.ID, owner, 64); err != nil {
		t.Fatal(err)
	}
	scanned, err := eng.stepConstraintValidation(ctx, transition.ID, owner, 64)
	if err != nil {
		t.Fatal(err)
	}
	if scanned.RowsScanned != 7 {
		t.Fatalf("rows scanned = %d, want 7", scanned.RowsScanned)
	}
	if _, _, violation, err := store.FirstTransitionViolation(ctx, eng.store, transition.ID); err != nil || !violation {
		t.Fatalf("historical NULL marker: violation=%v err=%v", violation, err)
	}
	if _, ok, err := eng.Update(
		ctx,
		"readings",
		lir.Row{"id": lir.Int64(20)},
		lir.Row{"value": lir.Text("repaired")},
	); err != nil || !ok {
		t.Fatalf("repair: ok=%v err=%v", ok, err)
	}
	if _, _, violation, err := store.FirstTransitionViolation(ctx, eng.store, transition.ID); err != nil || violation {
		t.Fatalf("repaired marker: violation=%v err=%v", violation, err)
	}
	ready := driveConstraintValidation(t, ctx, eng, transition.ID, owner, 8)
	if ready.State != model.TransitionReady {
		t.Fatalf("repaired validation = %+v", ready)
	}
}

func TestConstraintValidationFailureRemovesEnforcementExplicitly(t *testing.T) {
	eng, ctx := setupConstraintTable(t)
	if err := eng.Insert(ctx, "readings", lir.Row{"id": lir.Int64(20)}); err != nil {
		t.Fatal(err)
	}
	table, column := replacementColumn(t, ctx, eng, "readings", "value")
	transition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
		Name: "readings_value_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
	})
	if err != nil {
		t.Fatal(err)
	}
	failed, err := eng.runConstraintValidation(ctx, transition.ID, 2)
	if err == nil || failed.State != model.TransitionFailed ||
		failed.Constraint.State != model.ConstraintFailed ||
		!strings.Contains(failed.LastError, "is NULL") {
		t.Fatalf("failed validation = %+v err=%v", failed, err)
	}
	current, currentColumn := replacementColumn(t, ctx, eng, "readings", "value")
	if !currentColumn.Nullable {
		t.Fatalf("failed validation changed canonical nullability: %+v", currentColumn)
	}
	currentConstraint, ok := tableConstraint(current, failed.Constraint.ID)
	if !ok || currentConstraint.State != model.ConstraintFailed {
		t.Fatalf("failed constraint metadata = %+v ok=%v", currentConstraint, ok)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ConstraintChecks) != 0 || protocol.FinalizationGate != nil {
		t.Fatalf("failed constraint silently retained enforcement: %+v", protocol)
	}
	if err := eng.Insert(ctx, "readings", lir.Row{"id": lir.Int64(21)}); err != nil {
		t.Fatalf("failure policy did not restore nullable writes: %v", err)
	}
}

func TestConstraintValidationCancellationAndOwnerFencing(t *testing.T) {
	eng, ctx := setupConstraintTable(t)
	table, column := replacementColumn(t, ctx, eng, "readings", "value")
	transition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
		Name: "readings_value_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
	})
	if err != nil {
		t.Fatal(err)
	}
	ownerA, err := eng.claimConstraintValidation(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	ownerB, err := eng.claimConstraintValidation(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if ownerB <= ownerA {
		t.Fatalf("owner takeover = %d, old=%d", ownerB, ownerA)
	}
	if _, err := eng.stepConstraintValidation(ctx, transition.ID, ownerA, 1); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale owner step = %v, want conflict", err)
	}
	cancelled, err := eng.CancelSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != model.TransitionCancelled ||
		cancelled.Constraint.State != model.ConstraintCancelled ||
		cancelled.OwnerEpoch <= ownerB {
		t.Fatalf("cancelled validation = %+v", cancelled)
	}
	again, err := eng.CancelSchemaTransition(ctx, transition.ID)
	if err != nil || again.Generation != cancelled.Generation {
		t.Fatalf("idempotent cancellation = %+v err=%v", again, err)
	}
	current, _ := replacementColumn(t, ctx, eng, "readings", "value")
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ConstraintChecks) != 0 {
		t.Fatalf("cancelled constraint retained enforcement: %+v", protocol)
	}
}

func TestConstraintMetadataTracksReplacementAndActiveValidationBlocksColumnDeletion(t *testing.T) {
	t.Run("active validation blocks delete", func(t *testing.T) {
		eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
		table, column := replacementColumn(t, ctx, eng, "users", "age")
		transition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
			Name: "users_age_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
		})
		if err != nil {
			t.Fatal(err)
		}
		if _, err := eng.Catalog().DeleteColumn(ctx, "users", "age"); err == nil ||
			!strings.Contains(err.Error(), "active constraint transition") {
			t.Fatalf("delete during constraint validation = %v", err)
		}
		if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err != nil {
			t.Fatal(err)
		}
		deleted, err := eng.Catalog().DeleteColumn(ctx, "users", "age")
		if err != nil {
			t.Fatal(err)
		}
		if len(deleted.Constraints) != 0 {
			t.Fatalf("deleted column retained constraint metadata: %+v", deleted.Constraints)
		}
	})

	t.Run("valid constraint follows logical column replacement", func(t *testing.T) {
		eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
		table, column := replacementColumn(t, ctx, eng, "users", "age")
		constraintTransition, err := eng.startConstraintValidation(ctx, table.SchemaID, model.ConstraintDef{
			Name: "users_age_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
		})
		if err != nil {
			t.Fatal(err)
		}
		valid, err := eng.runConstraintValidation(ctx, constraintTransition.ID, 1)
		if err != nil {
			t.Fatal(err)
		}
		oldPhysicalID := valid.Constraint.ColumnIDs[0]
		table, column = replacementColumn(t, ctx, eng, "users", "age")
		replacement, err := eng.startColumnReplacement(ctx, table.SchemaID, column.SchemaID, model.ColumnReplacementDef{
			Type: model.TypeText, Nullable: false,
		})
		if err != nil {
			t.Fatal(err)
		}
		if _, err := eng.runColumnReplacement(ctx, replacement.ID, 1); err != nil {
			t.Fatal(err)
		}
		table, column = replacementColumn(t, ctx, eng, "users", "age")
		constraint, ok := tableConstraint(table, valid.Constraint.ID)
		if !ok || constraint.State != model.ConstraintValid ||
			len(constraint.ColumnIDs) != 1 ||
			constraint.ColumnIDs[0] != column.ID ||
			constraint.ColumnIDs[0] == oldPhysicalID {
			t.Fatalf("constraint did not follow replacement: constraint=%+v column=%+v", constraint, column)
		}
	})
}

func setupConstraintTable(t *testing.T) (*Engine, context.Context) {
	t.Helper()
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "readings",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	for i := range 6 {
		if err := eng.Insert(ctx, "readings", lir.Row{
			"id": lir.Int64(int64(i)), "value": lir.Text(fmt.Sprintf("value-%d", i)),
		}); err != nil {
			t.Fatal(err)
		}
	}
	return eng, ctx
}

func tableConstraint(table model.Table, id string) (model.Constraint, bool) {
	for _, constraint := range table.Constraints {
		if constraint.ID == id {
			return constraint, true
		}
	}
	return model.Constraint{}, false
}

func driveConstraintValidation(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
	transitionID string,
	owner uint64,
	batchSize int,
) model.SchemaTransition {
	t.Helper()
	for range 10_000 {
		transition, err := eng.stepConstraintValidation(ctx, transitionID, owner, batchSize)
		if errors.Is(err, kv.ErrConflict) {
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		switch transition.State {
		case model.TransitionReady, model.TransitionFailed:
			return transition
		}
	}
	t.Fatalf("constraint validation %q made no bounded progress", transitionID)
	return model.SchemaTransition{}
}
