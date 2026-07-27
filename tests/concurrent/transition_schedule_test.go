package concurrent

import (
	"context"
	"errors"
	"testing"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

func TestReplacementFinalizationScheduleIsExactlyReplayable(t *testing.T) {
	assertSemanticScheduleReplay(
		t,
		"replacement-gate-overtakes-writer.json",
		runReplacementGateSchedule,
		"ready:text:writer-conflicted",
	)
}

func runReplacementGateSchedule(t *testing.T, schedule []scheduleStep) ([]scheduleStep, string) {
	t.Helper()
	controller := newScheduleController()
	db := newChaosDB(
		t,
		exec.WithSchemaJobScheduling(false),
		exec.WithYieldHook(controller.hook),
	)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	t.Cleanup(cancel)
	table, err := db.Catalog.CreateTable(ctx, model.TableDef{
		ID: 61_000, Name: "scheduled_replacement",
		Columns: []model.ColumnDef{
			{ID: 1, Name: "id", Type: model.TypeInt64},
			{ID: 2, Name: "value", Type: model.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	source, ok := table.Column("value")
	if !ok {
		t.Fatal("replacement source column is missing")
	}
	started, err := db.Harness.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "replace", Kind: execprogram.StartColumnReplacement,
		TableID: table.SchemaID, ColumnID: source.SchemaID,
		Replacement: model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	}}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerStatement})
	if err != nil {
		t.Fatal(err)
	}
	if len(started.Statements) != 1 || started.Statements[0].Control == nil {
		t.Fatalf("start replacement = %+v", started.Statements)
	}
	transitionID := started.Statements[0].Control.TransitionID

	controller.arm()
	writerResult := make(chan error, 1)
	go func() {
		actorCtx := exec.WithYieldActor(ctx, "writer")
		writerResult <- db.Harness.Insert(actorCtx, table.Name, lir.Row{
			"id": lir.Int64(1), "value": lir.Int64(7),
		})
	}()
	runner := frontend.OpenWithOptions(
		db.store,
		exec.WithYieldHook(controller.hook),
		exec.WithSchemaJobConfig(exec.SchemaJobConfig{
			IndexBatchSize: 8, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
		}),
	)
	t.Cleanup(func() { _ = runner.Close() })

	if err := controller.drive(ctx, schedule); err != nil {
		t.Fatal(err)
	}
	writerErr := <-writerResult
	controller.disarm()
	if !errors.Is(writerErr, kv.ErrConflict) {
		t.Fatalf("writer crossing replacement gate = %v, want conflict", writerErr)
	}
	if _, found, err := db.Harness.GetByPrimaryKey(
		ctx,
		table.Name,
		lir.Row{"id": lir.Int64(1)},
	); err != nil || found {
		t.Fatalf("conflicted replacement row: found=%v err=%v", found, err)
	}
	ready, err := waitSchemaTransitionReady(ctx, db.Control, transitionID)
	if err != nil {
		t.Fatalf("publish replacement = %+v err=%v", ready, err)
	}
	published, ok, err := db.Catalog.GetTable(ctx, table.Name)
	if err != nil || !ok {
		t.Fatalf("published replacement table: found=%v err=%v", ok, err)
	}
	value, ok := published.Column("value")
	if !ok || value.Type != model.TypeText {
		t.Fatalf("published replacement column = %+v found=%v", value, ok)
	}
	return append([]scheduleStep(nil), controller.recorded...), "ready:text:writer-conflicted"
}

func TestConstraintEnforcementScheduleIsExactlyReplayable(t *testing.T) {
	assertSemanticScheduleReplay(
		t,
		"constraint-enforcement-overtakes-writer.json",
		runConstraintEnforcementSchedule,
		"ready:not-null:writer-conflicted",
	)
}

func runConstraintEnforcementSchedule(t *testing.T, schedule []scheduleStep) ([]scheduleStep, string) {
	t.Helper()
	controller := newScheduleController()
	db := newChaosDB(
		t,
		exec.WithSchemaJobScheduling(false),
		exec.WithYieldHook(controller.hook),
	)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	t.Cleanup(cancel)
	table, err := db.Catalog.CreateTable(ctx, model.TableDef{
		ID: 62_000, Name: "scheduled_constraint",
		Columns: []model.ColumnDef{
			{ID: 1, Name: "id", Type: model.TypeInt64},
			{ID: 2, Name: "value", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	value, ok := table.Column("value")
	if !ok {
		t.Fatal("constraint column is missing")
	}
	writer, err := db.Harness.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer writer.Rollback()
	if err := writer.Insert(ctx, table.Name, lir.Row{"id": lir.Int64(1)}); err != nil {
		t.Fatal(err)
	}

	controller.arm()
	writerResult := make(chan error, 1)
	starterResult := make(chan struct {
		transitionID string
		err          error
	}, 1)
	go func() {
		writerResult <- writer.Commit(exec.WithYieldActor(ctx, "writer"))
	}()
	go func() {
		actorCtx := exec.WithYieldActor(ctx, "constraint-starter")
		started, err := db.Harness.ExecuteProgram(actorCtx, execprogram.Program{Statements: []execprogram.Statement{{
			Name: "validate", Kind: execprogram.StartConstraintValidation,
			TableID: table.SchemaID,
			Constraint: model.ConstraintDef{
				Name: "scheduled_value_required",
				Kind: model.ConstraintNotNull, ColumnID: value.SchemaID,
			},
		}}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerStatement})
		transitionID := ""
		if err == nil {
			if len(started.Statements) != 1 || started.Statements[0].Control == nil {
				err = errors.New("start constraint returned no transition control")
			} else {
				transitionID = started.Statements[0].Control.TransitionID
			}
		}
		starterResult <- struct {
			transitionID string
			err          error
		}{transitionID: transitionID, err: err}
	}()

	if err := controller.drive(ctx, schedule); err != nil {
		t.Fatal(err)
	}
	writerErr := <-writerResult
	starter := <-starterResult
	controller.disarm()
	if starter.err != nil {
		t.Fatalf("start constraint validation: %v", starter.err)
	}
	if !errors.Is(writerErr, kv.ErrConflict) {
		t.Fatalf("writer crossing constraint enforcement = %v, want conflict", writerErr)
	}
	if _, found, err := db.Harness.GetByPrimaryKey(
		ctx,
		table.Name,
		lir.Row{"id": lir.Int64(1)},
	); err != nil || found {
		t.Fatalf("conflicted constraint row: found=%v err=%v", found, err)
	}
	runner := frontend.OpenWithOptions(db.store, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: 8, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	t.Cleanup(func() { _ = runner.Close() })
	ready, err := waitSchemaTransitionReady(ctx, db.Control, starter.transitionID)
	if err != nil {
		t.Fatalf("publish constraint = %+v err=%v", ready, err)
	}
	published, ok, err := db.Catalog.GetTable(ctx, table.Name)
	if err != nil || !ok {
		t.Fatalf("published constraint table: found=%v err=%v", ok, err)
	}
	value, ok = published.Column("value")
	if !ok || value.Nullable {
		t.Fatalf("published constraint column = %+v found=%v", value, ok)
	}
	if err := db.Harness.Insert(ctx, table.Name, lir.Row{"id": lir.Int64(2)}); err == nil {
		t.Fatal("published not-null constraint accepted an omitted value")
	}
	return append([]scheduleStep(nil), controller.recorded...),
		"ready:not-null:writer-conflicted"
}
