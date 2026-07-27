package exec

import (
	"context"
	"errors"
	"reflect"
	"slices"
	"strings"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestCompatibleColumnReplacementsComposeAndSerializeFinalization(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "measurements",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "left_value", Type: model.TypeInt64, Nullable: true},
			{Name: "right_value", Type: model.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, table.Name, lir.Row{
		"id": lir.Int64(1), "left_value": lir.Int64(10), "right_value": lir.Int64(20),
	}); err != nil {
		t.Fatal(err)
	}
	left, _ := table.Column("left_value")
	right, _ := table.Column("right_value")
	leftTransition, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		left.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	rightTransition, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		right.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	if rightTransition.State != model.TransitionBuilding {
		t.Fatalf("compatible second replacement = %+v", rightTransition)
	}
	current, ok, err := eng.Catalog().GetTable(ctx, table.Name)
	if err != nil || !ok {
		t.Fatalf("measurement table: found=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ColumnReplacements) != 2 ||
		protocol.ColumnReplacements[0].TransitionID >
			protocol.ColumnReplacements[1].TransitionID {
		t.Fatalf("composed replacement protocol = %+v", protocol.ColumnReplacements)
	}

	leftOwner, err := eng.claimColumnReplacement(ctx, leftTransition.ID)
	if err != nil {
		t.Fatal(err)
	}
	rightOwner, err := eng.claimColumnReplacement(ctx, rightTransition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepColumnReplacement(ctx, leftTransition.ID, leftOwner, 8); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.stepColumnReplacement(ctx, rightTransition.ID, rightOwner, 8); err != nil {
		t.Fatal(err)
	}
	leftValidating, err := eng.stepColumnReplacement(
		ctx,
		leftTransition.ID,
		leftOwner,
		8,
	)
	if err != nil {
		t.Fatal(err)
	}
	if leftValidating.State != model.TransitionValidating {
		t.Fatalf("left finalization state = %+v", leftValidating)
	}
	if _, err := eng.stepColumnReplacement(
		ctx,
		rightTransition.ID,
		rightOwner,
		8,
	); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("overlapping table finalization = %v, want retryable conflict", err)
	}
	leftReady, err := eng.stepColumnReplacement(
		ctx,
		leftTransition.ID,
		leftOwner,
		8,
	)
	if err != nil || leftReady.State != model.TransitionReady {
		t.Fatalf("left publication = %+v err=%v", leftReady, err)
	}
	rightValidating, err := eng.stepColumnReplacement(
		ctx,
		rightTransition.ID,
		rightOwner,
		8,
	)
	if err != nil || rightValidating.State != model.TransitionValidating {
		t.Fatalf("right finalization after gate release = %+v err=%v", rightValidating, err)
	}
	rightReady, err := eng.stepColumnReplacement(
		ctx,
		rightTransition.ID,
		rightOwner,
		8,
	)
	if err != nil || rightReady.State != model.TransitionReady {
		t.Fatalf("right publication = %+v err=%v", rightReady, err)
	}
	row, ok, err := eng.GetByPrimaryKey(ctx, table.Name, lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok ||
		!row["left_value"].Equal(lir.Text("10")) ||
		!row["right_value"].Equal(lir.Text("20")) {
		t.Fatalf("composed replacement row = %v found=%v err=%v", row, ok, err)
	}
}

func TestSameColumnReplacementChainWaitsAndRebindsPhysicalSource(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	if err := eng.Insert(ctx, "users", userRow(1, "chain", 42)); err != nil {
		t.Fatal(err)
	}
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	first, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	second, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{
			Type: model.TypeInt64, Nullable: true,
			Prerequisites: []string{first.ID, first.ID},
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	if second.State != model.TransitionWaiting ||
		second.ColumnReplacement != nil ||
		len(second.Prerequisites) != 1 {
		t.Fatalf("waiting replacement chain = %+v", second)
	}
	metrics, err := eng.schemaStorageMetrics(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if metrics.ActiveTransitions != 2 || metrics.WaitingTransitions != 1 {
		t.Fatalf("waiting transition metrics = %+v", metrics)
	}
	if _, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeFloat64, Nullable: true},
	); err == nil || !strings.Contains(err.Error(), "add it as a prerequisite") {
		t.Fatalf("unordered same-column replacement = %v", err)
	}

	firstOwner, err := eng.claimColumnReplacement(ctx, first.ID)
	if err != nil {
		t.Fatal(err)
	}
	firstReady := driveColumnReplacement(t, ctx, eng, first.ID, firstOwner, 2)
	second, err = eng.activateWaitingSchemaTransition(ctx, second.ID)
	if err != nil {
		t.Fatal(err)
	}
	if second.State != model.TransitionBuilding ||
		second.ColumnReplacement == nil ||
		second.ColumnReplacement.Source.ID != firstReady.ColumnReplacement.Target.ID ||
		second.ColumnReplacement.Target.ID == firstReady.ColumnReplacement.Target.ID {
		t.Fatalf("rebound dependent replacement = %+v first=%+v", second, firstReady)
	}
	secondOwner, err := eng.claimColumnReplacement(ctx, second.ID)
	if err != nil {
		t.Fatal(err)
	}
	secondReady := driveColumnReplacement(t, ctx, eng, second.ID, secondOwner, 2)
	if secondReady.State != model.TransitionReady {
		t.Fatalf("second replacement = %+v", secondReady)
	}
	row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok || !row["age"].Equal(lir.Int64(42)) {
		t.Fatalf("replacement chain row = %v found=%v err=%v", row, ok, err)
	}
}

func TestWaitingTransitionFailsWhenPrerequisiteIsCancelled(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	first, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	dependent, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{
			Type: model.TypeFloat64, Nullable: true,
			Prerequisites: []string{first.ID},
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, first.ID); err != nil {
		t.Fatal(err)
	}
	dependent, err = eng.activateWaitingSchemaTransition(ctx, dependent.ID)
	if err != nil {
		t.Fatal(err)
	}
	if dependent.State != model.TransitionFailed ||
		dependent.ColumnReplacement != nil ||
		!strings.Contains(dependent.LastError, "cancelled") {
		t.Fatalf("failed dependent = %+v", dependent)
	}
	current, ok, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("users table: found=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ColumnReplacements) != 0 {
		t.Fatalf("failed waiting transition published obligations: %+v", protocol)
	}
}

func TestConstraintWaitsForReplacementAndRebindsPhysicalColumn(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	if err := eng.Insert(ctx, "users", userRow(1, "dependent", 21)); err != nil {
		t.Fatal(err)
	}
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	constraint, err := eng.startConstraintValidation(
		ctx,
		table.SchemaID,
		model.ConstraintDef{
			Name: "users_age_after_replacement", Kind: model.ConstraintNotNull,
			ColumnID: age.SchemaID, Prerequisites: []string{replacement.ID},
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	if constraint.State != model.TransitionWaiting ||
		constraint.Constraint == nil ||
		constraint.Constraint.State != model.ConstraintDeclared {
		t.Fatalf("waiting constraint = %+v", constraint)
	}
	replacementOwner, err := eng.claimColumnReplacement(ctx, replacement.ID)
	if err != nil {
		t.Fatal(err)
	}
	replacementReady := driveColumnReplacement(
		t,
		ctx,
		eng,
		replacement.ID,
		replacementOwner,
		2,
	)
	constraint, err = eng.activateWaitingSchemaTransition(ctx, constraint.ID)
	if err != nil {
		t.Fatal(err)
	}
	if constraint.State != model.TransitionBuilding ||
		constraint.Constraint == nil ||
		constraint.Constraint.State != model.ConstraintEnforcingNewWrites ||
		len(constraint.Constraint.ColumnIDs) != 1 ||
		constraint.Constraint.ColumnIDs[0] !=
			replacementReady.ColumnReplacement.Target.ID {
		t.Fatalf(
			"activated constraint did not rebind to replacement target: constraint=%+v replacement=%+v",
			constraint,
			replacementReady,
		)
	}
	constraintOwner, err := eng.claimConstraintValidation(ctx, constraint.ID)
	if err != nil {
		t.Fatal(err)
	}
	constraintReady := driveConstraintValidation(
		t,
		ctx,
		eng,
		constraint.ID,
		constraintOwner,
		2,
	)
	if constraintReady.State != model.TransitionReady {
		t.Fatalf("dependent constraint = %+v", constraintReady)
	}
	_, publishedAge := replacementColumn(t, ctx, eng, "users", "age")
	if publishedAge.Nullable || publishedAge.Type != model.TypeText {
		t.Fatalf("dependent replacement/constraint column = %+v", publishedAge)
	}
}

func TestReplacementCannotLoosenValidNotNullConstraint(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	if err := eng.Insert(ctx, "users", userRow(1, "constrained", 18)); err != nil {
		t.Fatal(err)
	}
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	constraint, err := eng.startConstraintValidation(
		ctx,
		table.SchemaID,
		model.ConstraintDef{
			Name: "users_age_stays_required", Kind: model.ConstraintNotNull,
			ColumnID: age.SchemaID,
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimConstraintValidation(ctx, constraint.ID)
	if err != nil {
		t.Fatal(err)
	}
	ready := driveConstraintValidation(t, ctx, eng, constraint.ID, owner, 2)
	if ready.State != model.TransitionReady {
		t.Fatalf("constraint = %+v", ready)
	}
	if _, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	); err == nil || !strings.Contains(err.Error(), "valid constraint") {
		t.Fatalf("nullable replacement beneath valid constraint = %v", err)
	}
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: false},
	)
	if err != nil {
		t.Fatal(err)
	}
	replacementOwner, err := eng.claimColumnReplacement(ctx, replacement.ID)
	if err != nil {
		t.Fatal(err)
	}
	replacement = driveColumnReplacement(
		t,
		ctx,
		eng,
		replacement.ID,
		replacementOwner,
		2,
	)
	current, ok, err := eng.Catalog().GetTable(ctx, table.Name)
	if err != nil || !ok {
		t.Fatalf("users table after replacement: found=%v err=%v", ok, err)
	}
	currentAge, ok := current.Column("age")
	if !ok || currentAge.Nullable || currentAge.ID != replacement.ColumnReplacement.Target.ID {
		t.Fatalf("replacement target = %+v transition=%+v", currentAge, replacement)
	}
	constraintIndex := slices.IndexFunc(current.Constraints, func(candidate model.Constraint) bool {
		return candidate.ID == ready.Constraint.ID
	})
	if constraintIndex < 0 ||
		!reflect.DeepEqual(current.Constraints[constraintIndex].ColumnIDs, []string{currentAge.ID}) {
		t.Fatalf("valid constraint did not rebind to replacement: %+v", current.Constraints)
	}
}

func TestWaitingConstraintCancellationPublishesNoWriteObligation(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	constraint, err := eng.startConstraintValidation(
		ctx,
		table.SchemaID,
		model.ConstraintDef{
			Name: "users_age_cancelled_waiter", Kind: model.ConstraintNotNull,
			ColumnID: age.SchemaID, Prerequisites: []string{replacement.ID},
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	cancelled, err := eng.CancelSchemaTransition(ctx, constraint.ID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != model.TransitionCancelled ||
		cancelled.Constraint == nil ||
		cancelled.Constraint.State != model.ConstraintCancelled {
		t.Fatalf("cancelled waiting constraint = %+v", cancelled)
	}
	again, err := eng.CancelSchemaTransition(ctx, constraint.ID)
	if err != nil || again.Generation != cancelled.Generation {
		t.Fatalf("idempotent waiting cancellation = %+v err=%v", again, err)
	}
	current, ok, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("users table: found=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ConstraintChecks) != 0 ||
		len(protocol.ColumnReplacements) != 1 ||
		protocol.ColumnReplacements[0].TransitionID != replacement.ID {
		t.Fatalf("waiting cancellation disturbed composed protocol: %+v", protocol)
	}
}

func TestIndexBuildAndConstraintValidationComposeOnSameColumn(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	if err := eng.Insert(ctx, "users", userRow(1, "composed", 12)); err != nil {
		t.Fatal(err)
	}
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	index, err := eng.startIndexBuild(
		ctx,
		"users",
		model.IndexDef{Name: "users_age_composed", Columns: []string{"age"}},
	)
	if err != nil {
		t.Fatal(err)
	}
	constraint, err := eng.startConstraintValidation(
		ctx,
		table.SchemaID,
		model.ConstraintDef{
			Name: "users_age_required", Kind: model.ConstraintNotNull,
			ColumnID: age.SchemaID,
		},
	)
	if err != nil {
		t.Fatalf("index/constraint compatibility: %v", err)
	}
	current, ok, err := eng.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("users table: found=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.DeltaSinks) != 1 || len(protocol.ConstraintChecks) != 1 {
		t.Fatalf("composed index/constraint protocol = %+v", protocol)
	}
	if err := eng.Insert(ctx, "users", lir.Row{
		"id": lir.Int64(2), "name": lir.Text("null"),
	}); err == nil || !strings.Contains(err.Error(), "rejects NULL") {
		t.Fatalf("composed foreground constraint = %v", err)
	}
	if err := eng.Insert(ctx, "users", userRow(2, "captured", 13)); err != nil {
		t.Fatal(err)
	}
	indexOwner, err := eng.claimIndexBuild(ctx, index.ID)
	if err != nil {
		t.Fatal(err)
	}
	indexReady := driveIndexBuild(t, ctx, eng, index.ID, indexOwner, 2)
	if indexReady.State != model.TransitionReady {
		t.Fatalf("composed index = %+v", indexReady)
	}
	constraintOwner, err := eng.claimConstraintValidation(ctx, constraint.ID)
	if err != nil {
		t.Fatal(err)
	}
	constraintReady := driveConstraintValidation(
		t,
		ctx,
		eng,
		constraint.ID,
		constraintOwner,
		2,
	)
	if constraintReady.State != model.TransitionReady {
		t.Fatalf("composed constraint = %+v", constraintReady)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_age_composed")
}

func TestIndexBuildWaitsForReplacementAndResolvesRenamedLogicalColumn(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	if err := eng.Insert(ctx, "users", userRow(1, "indexed-after", 14)); err != nil {
		t.Fatal(err)
	}
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	index, err := eng.startIndexBuildBySchemaIDWithPrerequisites(
		ctx,
		table.SchemaID,
		model.IndexDef{Name: "users_age_after_replacement", Columns: []string{"age"}},
		[]string{replacement.ID},
	)
	if err != nil {
		t.Fatal(err)
	}
	if index.State != model.TransitionWaiting ||
		index.IndexRequest == nil ||
		!slices.Equal(index.IndexRequest.ColumnSchemaIDs, []model.SchemaID{age.SchemaID}) ||
		len(index.Index.ColumnIDs) != 0 {
		t.Fatalf("waiting index = %+v", index)
	}
	if _, err := eng.startIndexBuild(
		ctx,
		"users",
		model.IndexDef{Name: "users_age_after_replacement", Columns: []string{"age"}},
	); err == nil || !strings.Contains(err.Error(), "reserved") {
		t.Fatalf("waiting index did not reserve its name: %v", err)
	}
	if _, err := eng.Catalog().RenameColumn(ctx, "users", "age", "years"); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().CreateColumn(ctx, "users", model.ColumnDef{
		Name: "age", Type: model.TypeInt64, Nullable: true,
	}); err != nil {
		t.Fatal(err)
	}
	replacementOwner, err := eng.claimColumnReplacement(ctx, replacement.ID)
	if err != nil {
		t.Fatal(err)
	}
	replacementReady := driveColumnReplacement(
		t,
		ctx,
		eng,
		replacement.ID,
		replacementOwner,
		2,
	)
	index, err = eng.activateWaitingSchemaTransition(ctx, index.ID)
	if err != nil {
		t.Fatal(err)
	}
	if index.State != model.TransitionBuilding ||
		len(index.Index.Columns) != 1 ||
		index.Index.Columns[0] != "years" ||
		len(index.Index.ColumnIDs) != 1 ||
		index.Index.ColumnIDs[0] != replacementReady.ColumnReplacement.Target.ID {
		t.Fatalf(
			"activated index did not resolve replacement/rename: index=%+v replacement=%+v",
			index,
			replacementReady,
		)
	}
	indexOwner, err := eng.claimIndexBuild(ctx, index.ID)
	if err != nil {
		t.Fatal(err)
	}
	indexReady := driveIndexBuild(t, ctx, eng, index.ID, indexOwner, 2)
	if indexReady.State != model.TransitionReady {
		t.Fatalf("dependent index = %+v", indexReady)
	}
	assertIndexEqualsTable(
		t,
		ctx,
		eng,
		"users",
		"users_age_after_replacement",
	)
}

func TestCancellingWaitingIndexReleasesReservedName(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	index, err := eng.startIndexBuildBySchemaIDWithPrerequisites(
		ctx,
		table.SchemaID,
		model.IndexDef{Name: "users_reserved_waiting", Columns: []string{"age"}},
		[]string{replacement.ID},
	)
	if err != nil {
		t.Fatal(err)
	}
	cancelled, err := eng.CancelSchemaTransition(ctx, index.ID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != model.TransitionCancelled ||
		cancelled.Index.State != model.IndexCancelled ||
		len(cancelled.Index.ColumnIDs) != 0 {
		t.Fatalf("cancelled waiting index = %+v", cancelled)
	}
	if _, err := eng.startIndexBuild(
		ctx,
		"users",
		model.IndexDef{Name: "users_reserved_waiting", Columns: []string{"name"}},
	); err != nil {
		t.Fatalf("cancelled waiting index retained name reservation: %v", err)
	}
}

func TestFailedWaitingIndexReleasesNameAndRetiresIdentities(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	waiting, err := eng.startIndexBuildBySchemaIDWithPrerequisites(
		ctx,
		table.SchemaID,
		model.IndexDef{Name: "users_retry_waiting", Columns: []string{"age"}},
		[]string{replacement.ID},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, replacement.ID); err != nil {
		t.Fatal(err)
	}
	waiting, err = eng.activateWaitingSchemaTransition(ctx, waiting.ID)
	if err != nil {
		t.Fatal(err)
	}
	if waiting.State != model.TransitionFailed {
		t.Fatalf("dependent index after prerequisite cancellation = %+v", waiting)
	}

	retry, err := eng.startIndexBuild(
		ctx,
		"users",
		model.IndexDef{Name: "users_retry_waiting", Columns: []string{"age"}},
	)
	if err != nil {
		t.Fatalf("failed waiter retained name reservation: %v", err)
	}
	if retry.ID == waiting.ID ||
		retry.Index.LogicalID == waiting.Index.LogicalID ||
		retry.Index.ID == waiting.Index.ID {
		t.Fatalf("retry reused retired transition/index identities: failed=%+v retry=%+v", waiting, retry)
	}
}

func TestWaitingIndexActivationRejectsDuplicateNameReservation(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	replacement, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	index, err := eng.startIndexBuildBySchemaIDWithPrerequisites(
		ctx,
		table.SchemaID,
		model.IndexDef{Name: "users_corrupt_reservation", Columns: []string{"age"}},
		[]string{replacement.ID},
	)
	if err != nil {
		t.Fatal(err)
	}
	owner, err := eng.claimColumnReplacement(ctx, replacement.ID)
	if err != nil {
		t.Fatal(err)
	}
	driveColumnReplacement(t, ctx, eng, replacement.ID, owner, 2)

	txn, err := eng.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	duplicate := index
	duplicate.ID = "tr-corrupt-reservation"
	if err := store.SaveTransition(ctx, txn, duplicate); err != nil {
		txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}

	index, err = eng.activateWaitingSchemaTransition(ctx, index.ID)
	if err != nil {
		t.Fatal(err)
	}
	if index.State != model.TransitionFailed ||
		!strings.Contains(index.LastError, "reserved by both") {
		t.Fatalf("activation admitted duplicate name reservation: %+v", index)
	}
}

func TestReplacementCannotWaitBehindIndexThatWillRetainOldPhysicalColumn(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, target, blocker := createComposableGuardTable(t, ctx, eng)
	prerequisite, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		blocker.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	index, err := eng.startIndexBuildBySchemaIDWithPrerequisites(
		ctx,
		table.SchemaID,
		model.IndexDef{Name: "guard_target_blocks_replacement", Columns: []string{"target"}},
		[]string{prerequisite.ID},
	)
	if err != nil {
		t.Fatal(err)
	}
	_, err = eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		target.SchemaID,
		model.ColumnReplacementDef{
			Type: model.TypeText, Nullable: true,
			Prerequisites: []string{index.ID},
		},
	)
	if err == nil ||
		!strings.Contains(err.Error(), "ready index would still depend on the old physical column") {
		t.Fatalf("replacement behind durable index dependency = %v", err)
	}
}

func TestWaitingIndexPreventsLogicalColumnDeletion(t *testing.T) {
	eng, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		withAutomaticReclamation(false),
	)
	table, _, blocker := createComposableGuardTable(t, ctx, eng)
	prerequisite, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		blocker.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	index, err := eng.startIndexBuildBySchemaIDWithPrerequisites(
		ctx,
		table.SchemaID,
		model.IndexDef{Name: "guard_target_waiting_delete", Columns: []string{"target"}},
		[]string{prerequisite.ID},
	)
	if err != nil {
		t.Fatal(err)
	}
	if index.State != model.TransitionWaiting {
		t.Fatalf("index state = %q, want waiting", index.State)
	}
	if _, err := eng.Catalog().DeleteColumn(ctx, table.Name, "target"); err == nil ||
		!strings.Contains(err.Error(), "active index transition") {
		t.Fatalf("delete column guarded by waiting index = %v", err)
	}
}

func createComposableGuardTable(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
) (model.Table, model.Column, model.Column) {
	t.Helper()
	table, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "transition_guards",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "target", Type: model.TypeInt64, Nullable: true},
			{Name: "blocker", Type: model.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	target, ok := table.Column("target")
	if !ok {
		t.Fatal("transition_guards.target is missing")
	}
	blocker, ok := table.Column("blocker")
	if !ok {
		t.Fatal("transition_guards.blocker is missing")
	}
	return table, target, blocker
}
