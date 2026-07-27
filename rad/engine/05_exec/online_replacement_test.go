package exec

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

func TestOnlineColumnReplacementDualWritesBackfillsAndSwitchesAtomically(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	for i := range 9 {
		row := userRow(i, fmt.Sprintf("user-%d", i), int64(i))
		if i == 4 {
			row["age"] = lir.Null(model.TypeInt64)
		}
		if err := eng.Insert(ctx, "users", row); err != nil {
			t.Fatal(err)
		}
	}
	table, source := replacementColumn(t, ctx, eng, "users", "age")
	transition, err := eng.startColumnReplacement(ctx, table.SchemaID, source.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeText, Nullable: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if transition.State != model.TransitionBuilding ||
		transition.ColumnReplacement.Source.ID != source.ID ||
		transition.ColumnReplacement.Target.ID == source.ID ||
		transition.ColumnReplacement.Target.SchemaID != source.SchemaID {
		t.Fatalf("replacement identity = %+v", transition)
	}

	table, _ = replacementColumn(t, ctx, eng, "users", "age")
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, table)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ColumnReplacements) != 1 ||
		protocol.ColumnReplacements[0].TransitionID != transition.ID {
		t.Fatalf("published write protocol = %+v", protocol)
	}
	current, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(2)})
	if err != nil || !ok || !current["age"].Equal(lir.Int64(2)) {
		t.Fatalf("source was not the bindable representation: row=%v ok=%v err=%v", current, ok, err)
	}

	owner, err := eng.claimColumnReplacement(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	partial, err := eng.stepColumnReplacement(ctx, transition.ID, owner, 2)
	if err != nil {
		t.Fatal(err)
	}
	if partial.RowsScanned != 2 || len(partial.Cursor) == 0 {
		t.Fatalf("partial checkpoint = %+v", partial)
	}
	if err := eng.Insert(ctx, "users", userRow(100, "concurrent", 55)); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := eng.Update(
		ctx,
		"users",
		lir.Row{"id": lir.Int64(3)},
		lir.Row{"age": lir.Int64(88)},
	); err != nil || !ok {
		t.Fatalf("concurrent update: ok=%v err=%v", ok, err)
	}
	assertReplacementCells(t, ctx, eng, table, transition, 100, lir.Int64(55), lir.Text("55"))

	ready := driveColumnReplacement(t, ctx, eng, transition.ID, owner, 3)
	if ready.State != model.TransitionReady || ready.RowsScanned != 10 {
		t.Fatalf("ready replacement = %+v", ready)
	}
	published, target := replacementColumn(t, ctx, eng, "users", "age")
	if target.ID != transition.ColumnReplacement.Target.ID ||
		target.ID == source.ID ||
		target.SchemaID != source.SchemaID ||
		target.Type != model.TypeText {
		t.Fatalf("published target = %+v, source=%+v", target, source)
	}
	protocol, err = store.ReadWriteProtocol(ctx, eng.store, published)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ColumnReplacements) != 0 || protocol.FinalizationGate != nil {
		t.Fatalf("terminal protocol retained replacement work: %+v", protocol)
	}

	want := map[int]lir.Value{
		0: lir.Text("0"), 1: lir.Text("1"), 2: lir.Text("2"), 3: lir.Text("88"),
		4: lir.Null(model.TypeText), 5: lir.Text("5"), 6: lir.Text("6"),
		7: lir.Text("7"), 8: lir.Text("8"), 100: lir.Text("55"),
	}
	for id, expected := range want {
		row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(int64(id))})
		if err != nil || !ok || row["age"] != expected {
			t.Fatalf("row %d after publication = %v ok=%v err=%v, want age=%v", id, row, ok, err, expected)
		}
	}
	if err := eng.Insert(ctx, "users", lir.Row{
		"id": lir.Int64(101), "name": lir.Text("new-type"), "age": lir.Text("101"),
	}); err != nil {
		t.Fatalf("new physical type rejected after publication: %v", err)
	}
	if err := eng.Insert(ctx, "users", userRow(102, "old-type", 102)); err == nil ||
		!strings.Contains(err.Error(), "expects text") {
		t.Fatalf("old physical type accepted after publication: %v", err)
	}
}

func TestOnlineColumnReplacementFencesWriterAcrossPublication(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	if err := eng.Insert(ctx, "users", userRow(1, "base", 1)); err != nil {
		t.Fatal(err)
	}
	table, source := replacementColumn(t, ctx, eng, "users", "age")
	transition, err := eng.startColumnReplacement(ctx, table.SchemaID, source.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeText, Nullable: true,
	})
	if err != nil {
		t.Fatal(err)
	}

	writer, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer writer.Rollback()
	if err := writer.Insert(ctx, "users", userRow(2, "racing", 2)); err != nil {
		t.Fatal(err)
	}
	ready, err := eng.runColumnReplacement(ctx, transition.ID, 1)
	if err != nil {
		t.Fatal(err)
	}
	if ready.State != model.TransitionReady {
		t.Fatalf("replacement state = %q", ready.State)
	}
	if err := writer.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("writer admitted under old protocol committed after publication: %v", err)
	}
	if _, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(2)}); err != nil || ok {
		t.Fatalf("conflicted row visible: ok=%v err=%v", ok, err)
	}
}

func TestOnlineColumnReplacementFailurePreservesSourceAndCleansProtocol(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	table, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "measurements",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	for id, value := range []lir.Value{lir.Text("42"), lir.Text("not-an-int"), lir.Null(model.TypeText)} {
		if err := eng.Insert(ctx, "measurements", lir.Row{
			"id": lir.Int64(int64(id)), "value": value,
		}); err != nil {
			t.Fatal(err)
		}
	}
	source, ok := table.Column("value")
	if !ok {
		t.Fatal("measurements.value does not exist")
	}
	transition, err := eng.startColumnReplacement(ctx, table.SchemaID, source.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeInt64, Nullable: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	failed, err := eng.runColumnReplacement(ctx, transition.ID, 1)
	if err == nil || failed.State != model.TransitionFailed ||
		!strings.Contains(failed.LastError, "not-an-int") {
		t.Fatalf("failed replacement = %+v err=%v", failed, err)
	}
	current, active := replacementColumn(t, ctx, eng, "measurements", "value")
	if active.ID != source.ID || active.Type != model.TypeText {
		t.Fatalf("failed replacement changed logical schema: source=%+v active=%+v", source, active)
	}
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ColumnReplacements) != 0 || protocol.FinalizationGate != nil {
		t.Fatalf("failed replacement retained write obligations: %+v", protocol)
	}
	if err := eng.Insert(ctx, "measurements", lir.Row{
		"id": lir.Int64(9), "value": lir.Text("source-still-active"),
	}); err != nil {
		t.Fatalf("source representation unusable after failure: %v", err)
	}
	reclamationID := store.FailedReplacementReclamationID(transition.ID)
	reclamation, ok, err := store.GetReclamation(ctx, eng.store, reclamationID)
	if err != nil || !ok || reclamation.Kind != model.ReclamationFailedReplacement {
		t.Fatalf("failed replacement cleanup = %+v ok=%v err=%v", reclamation, ok, err)
	}
	reclamationOwner, claimed, err := eng.claimReclamation(ctx, reclamationID)
	if err != nil || !claimed {
		t.Fatalf("claim failed replacement cleanup: claimed=%v err=%v", claimed, err)
	}
	if err := eng.runReclamation(ctx, reclamationID, reclamationOwner, 1); err != nil {
		t.Fatal(err)
	}
}

func TestOnlineColumnReplacementOwnerTakeoverCancellationAndAdmission(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	table, source := replacementColumn(t, ctx, eng, "users", "age")
	transition, err := eng.startColumnReplacement(ctx, table.SchemaID, source.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeText, Nullable: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	ownerA, err := eng.claimColumnReplacement(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	reopened := New(eng.store, catalog.New(eng.store), WithSchemaJobScheduling(false))
	t.Cleanup(func() { _ = reopened.Close() })
	ownerB, err := reopened.claimColumnReplacement(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if ownerB <= ownerA {
		t.Fatalf("takeover epoch = %d, old owner = %d", ownerB, ownerA)
	}
	if _, err := eng.stepColumnReplacement(ctx, transition.ID, ownerA, 1); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale owner step = %v, want conflict", err)
	}
	cancelled, err := reopened.CancelSchemaTransition(ctx, transition.ID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != model.TransitionCancelled || cancelled.OwnerEpoch <= ownerB {
		t.Fatalf("cancelled replacement = %+v", cancelled)
	}
	table, _ = replacementColumn(t, ctx, eng, "users", "age")
	protocol, err := store.ReadWriteProtocol(ctx, eng.store, table)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.ColumnReplacements) != 0 {
		t.Fatalf("cancelled replacement retained write protocol: %+v", protocol)
	}

	for _, columnName := range []string{"id", "name"} {
		_, column := replacementColumn(t, ctx, eng, "users", columnName)
		if _, err := eng.startColumnReplacement(
			ctx,
			table.SchemaID,
			column.SchemaID,
			model.ColumnReplacementDef{Type: model.TypeText},
		); err == nil {
			t.Fatalf("replacement of dependency column %q was admitted", columnName)
		}
	}
}

func TestColumnReplacementPrerequisitesWaitAndActivateFromCanonicalDAG(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	users, age := replacementColumn(t, ctx, eng, "users", "age")
	orders, total := replacementColumn(t, ctx, eng, "orders", "total")
	first, err := eng.startColumnReplacement(ctx, orders.SchemaID, total.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeText, Nullable: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	second, err := eng.startColumnReplacement(ctx, users.SchemaID, age.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeText, Nullable: true, Prerequisites: []string{first.ID, first.ID},
	})
	if err != nil {
		t.Fatal(err)
	}
	if second.State != model.TransitionWaiting || second.ColumnReplacement != nil {
		t.Fatalf("waiting dependent replacement = %+v", second)
	}
	if _, err := eng.runColumnReplacement(ctx, first.ID, 1); err != nil {
		t.Fatal(err)
	}
	second, err = eng.activateWaitingSchemaTransition(ctx, second.ID)
	if err != nil {
		t.Fatal(err)
	}
	if second.State != model.TransitionBuilding || second.ColumnReplacement == nil {
		t.Fatalf("activated dependent replacement = %+v", second)
	}
	if len(second.Prerequisites) != 1 || second.Prerequisites[0] != first.ID {
		t.Fatalf("canonical prerequisites = %v", second.Prerequisites)
	}
	if _, err := eng.CancelSchemaTransition(ctx, second.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.startColumnReplacement(ctx, users.SchemaID, age.SchemaID, model.ColumnReplacementDef{
		Type: model.TypeText, Nullable: true, Prerequisites: []string{"tr-does-not-exist"},
	}); err == nil || !strings.Contains(err.Error(), "does not exist") {
		t.Fatalf("missing prerequisite = %v", err)
	}
}

func TestColumnDefaultChangeRejectsActiveReplacement(t *testing.T) {
	eng, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	table, age := replacementColumn(t, ctx, eng, "users", "age")
	transition, err := eng.startColumnReplacement(
		ctx,
		table.SchemaID,
		age.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().ChangeColumnInsertDefault(
		ctx,
		"users",
		"age",
		&model.Default{Int64: 18},
	); err == nil || !strings.Contains(err.Error(), "active replacement") {
		t.Fatalf("default change during replacement = %v", err)
	}
	if _, err := eng.CancelSchemaTransition(ctx, transition.ID); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().ChangeColumnInsertDefault(
		ctx,
		"users",
		"age",
		&model.Default{Int64: 18},
	); err != nil {
		t.Fatalf("default change after cancellation: %v", err)
	}
}

func replacementColumn(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
	tableName string,
	columnName string,
) (model.Table, model.Column) {
	t.Helper()
	table, ok, err := eng.Catalog().GetTable(ctx, tableName)
	if err != nil || !ok {
		t.Fatalf("get table %q: ok=%v err=%v", tableName, ok, err)
	}
	column, ok := table.Column(columnName)
	if !ok {
		t.Fatalf("table %q has no column %q", tableName, columnName)
	}
	return table, column
}

func assertReplacementCells(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
	table model.Table,
	transition model.SchemaTransition,
	id int64,
	wantSource lir.Value,
	wantTarget lir.Value,
) {
	t.Helper()
	pk, err := codec.EncodeRowTuple(lir.Row{"id": lir.Int64(id)}, table.PrimaryKey)
	if err != nil {
		t.Fatal(err)
	}
	raw, ok, err := eng.store.Get(ctx, codec.DataKey(table.ID, pk))
	if err != nil || !ok {
		t.Fatalf("raw row %d: ok=%v err=%v", id, ok, err)
	}
	source, err := codec.ReadColumnValue(raw, transition.ColumnReplacement.Source)
	if err != nil || source != wantSource {
		t.Fatalf("source cell %d = %+v err=%v, want %+v", id, source, err, wantSource)
	}
	target, err := codec.ReadColumnValue(raw, transition.ColumnReplacement.Target)
	if err != nil || target != wantTarget {
		t.Fatalf("target cell %d = %+v err=%v, want %+v", id, target, err, wantTarget)
	}
}

func driveColumnReplacement(
	t *testing.T,
	ctx context.Context,
	eng *Engine,
	transitionID string,
	owner uint64,
	batchSize int,
) model.SchemaTransition {
	t.Helper()
	for range 10_000 {
		transition, err := eng.stepColumnReplacement(ctx, transitionID, owner, batchSize)
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
	t.Fatalf("replacement %q made no bounded progress", transitionID)
	return model.SchemaTransition{}
}
