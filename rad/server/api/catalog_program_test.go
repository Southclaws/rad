package api

import (
	"context"
	"encoding/json"
	"reflect"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestExecuteCatalogProgramCreatesSchemaThenUsesIt(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, columnID := pirwire.SchemaID(1), pirwire.SchemaID(1)

	program := pirwire.Prog("all",
		pirwire.CreateTable("schema", pirwire.TableDefinition{
			ID: &tableID, Name: "notes",
			Columns:    []pirwire.ColumnDefinition{{ID: &columnID, Name: "id", Type: pirwire.ColumnTypeInt64}},
			PrimaryKey: []string{"id"},
		}),
		pirwire.Create("seed", "notes", relBytes(rowsRel("r",
			tcol("id", "int64"), [][]lirwire.Cell{{mustValue(1)}, {mustValue(2)}}))),
		pirwire.Query("all", relBytes(scanOrdered("notes", "n", "id"))),
	)
	result, err := c.Execute(ctx, program)
	if err != nil {
		t.Fatal(err)
	}
	rows, ok := result.Result.([]any)
	if !ok || len(rows) != 2 {
		t.Fatalf("result = %#v, want two rows", result.Result)
	}
	if len(result.Statements) != 3 || result.Statements[0].Affected != 1 || result.Statements[1].Affected != 2 {
		t.Fatalf("summary = %#v", result.Statements)
	}
	info, err := c.Info(ctx)
	if err != nil || info.SchemaVersion != 1 {
		t.Fatalf("info = %+v, %v", info, err)
	}
}

func TestExecuteCatalogProgramUsesOneRevisionPerDirectStatement(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, bodyColumn := pirwire.SchemaID(7), pirwire.SchemaID(1), pirwire.SchemaID(2)

	result, err := c.Execute(ctx, pirwire.Prog("",
		pirwire.CreateTable("table", pirwire.TableDefinition{
			ID: &tableID, Name: "notes",
			Columns:    []pirwire.ColumnDefinition{{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64}},
			PrimaryKey: []string{"id"},
		}),
		pirwire.CreateColumn("body", tableID, pirwire.ColumnDefinition{
			ID: &bodyColumn, Name: "body", Type: pirwire.ColumnTypeText, Nullable: ptrBool(true),
		}),
	))
	if err != nil {
		t.Fatal(err)
	}
	if result.Result != nil || len(result.Statements) != 2 {
		t.Fatalf("catalog-only result = %#v", result)
	}
	info, err := c.Info(ctx)
	if err != nil || info.SchemaVersion != 2 {
		t.Fatalf("info = %+v, %v", info, err)
	}
}

func TestExecuteCatalogProgramChangesOnlyFutureInsertDefault(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, statusColumn := pirwire.SchemaID(17), pirwire.SchemaID(1), pirwire.SchemaID(2)
	active := &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{
		Kind: "literal", Value: json.RawMessage(`"active"`),
	}}
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("table", pirwire.TableDefinition{
		ID: &tableID, Name: "items",
		Columns: []pirwire.ColumnDefinition{
			{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
			{
				ID: &statusColumn, Name: "status", Type: pirwire.ColumnTypeText,
				Nullable: ptrBool(true), Default: active,
			},
		},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.Create(
		"old",
		"items",
		relBytes(rowsRel("r", tcol("id", "int64"), [][]lirwire.Cell{{mustValue(1)}})),
	))); err != nil {
		t.Fatal(err)
	}

	wrongType := &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{
		Kind: "literal", Value: json.RawMessage(`42`),
	}}
	if _, err := c.Execute(ctx, pirwire.Prog(
		"",
		pirwire.ChangeColumnDefault("wrong", tableID, statusColumn, wrongType),
	)); err == nil {
		t.Fatal("numeric text default was accepted")
	}
	info, err := c.Info(ctx)
	if err != nil || info.SchemaVersion != 1 {
		t.Fatalf("failed default change advanced schema: info=%+v err=%v", info, err)
	}
	tables, err := c.Tables(ctx)
	if err != nil || len(tables) != 1 || tables[0].Columns[1].Default == nil ||
		tables[0].Columns[1].Default.Value != "active" {
		t.Fatalf("failed default change mutated catalog: tables=%+v err=%v", tables, err)
	}

	pending := &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{
		Kind: "literal", Value: json.RawMessage(`"pending"`),
	}}
	result, err := c.Execute(ctx, pirwire.Prog("all",
		pirwire.ChangeColumnDefault("default", tableID, statusColumn, pending),
		pirwire.Create(
			"new",
			"items",
			relBytes(rowsRel("r", tcol("id", "int64"), [][]lirwire.Cell{{mustValue(2)}})),
		),
		pirwire.Query("all", relBytes(scanOrdered("items", "i", "id"))),
	))
	if err != nil {
		t.Fatal(err)
	}
	rows, ok := result.Result.([]any)
	if !ok || len(rows) != 2 {
		t.Fatalf("result = %#v", result.Result)
	}
	first := rows[0].(map[string]any)
	second := rows[1].(map[string]any)
	if first["status"] != "active" || second["status"] != "pending" {
		t.Fatalf("statuses after default change = %#v", rows)
	}

	if _, err := c.Execute(ctx, pirwire.Prog("cleared",
		pirwire.ChangeColumnDefault("clear", tableID, statusColumn, nil),
		pirwire.Create(
			"cleared",
			"items",
			relBytes(rowsRel("r", tcol("id", "int64"), [][]lirwire.Cell{{mustValue(3)}})),
		),
	)); err != nil {
		t.Fatal(err)
	}
	queried, err := c.Query(ctx, scanOrdered("items", "i", "id"))
	if err != nil {
		t.Fatal(err)
	}
	if queried[0]["status"] != "active" || queried[1]["status"] != "pending" ||
		queried[2]["status"] != nil {
		t.Fatalf("statuses after clearing default = %#v", queried)
	}
}

func TestExecuteCatalogProgramRenameIsVisibleToLaterQuery(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, columnID := pirwire.SchemaID(1), pirwire.SchemaID(1)
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("create", pirwire.TableDefinition{
		ID: &tableID, Name: "users",
		Columns:    []pirwire.ColumnDefinition{{ID: &columnID, Name: "id", Type: pirwire.ColumnTypeInt64}},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}

	result, err := c.Execute(ctx, pirwire.Prog("accounts",
		pirwire.RenameTable("rename", tableID, "accounts"),
		pirwire.Query("accounts", relBytes(scanOrdered("accounts", "a", "id"))),
	))
	if err != nil {
		t.Fatal(err)
	}
	if rows, ok := result.Result.([]any); !ok || len(rows) != 0 {
		t.Fatalf("result = %#v, want empty rows", result.Result)
	}
	if tables, err := c.Tables(ctx); err != nil || len(tables) != 1 || tables[0].Name != "accounts" {
		t.Fatalf("tables = %#v, %v", tables, err)
	}
}

func TestExecuteCatalogProgramRollsBackSchemaAndRevision(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, columnID := pirwire.SchemaID(1), pirwire.SchemaID(1)

	_, err := c.Execute(ctx, pirwire.Prog("bad",
		pirwire.CreateTable("schema", pirwire.TableDefinition{
			ID: &tableID, Name: "notes",
			Columns:    []pirwire.ColumnDefinition{{ID: &columnID, Name: "id", Type: pirwire.ColumnTypeInt64}},
			PrimaryKey: []string{"id"},
		}),
		pirwire.Query("bad", relBytes(scanOrdered("missing", "m", "id"))),
	))
	if err == nil {
		t.Fatal("program should fail while binding the query")
	}
	tables, tableErr := c.Tables(ctx)
	info, infoErr := c.Info(ctx)
	if tableErr != nil || infoErr != nil || len(tables) != 0 || info.SchemaVersion != 0 {
		t.Fatalf("rollback left tables=%#v info=%+v errors=(%v, %v)", tables, info, tableErr, infoErr)
	}
}

func TestExecuteCatalogIndexBackfillSeesEarlierProgramWrites(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, valueColumn := pirwire.SchemaID(1), pirwire.SchemaID(1), pirwire.SchemaID(2)
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("schema", pirwire.TableDefinition{
		ID: &tableID, Name: "items",
		Columns: []pirwire.ColumnDefinition{
			{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
			{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
		},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}

	_, err := c.Execute(ctx, pirwire.Prog("seed",
		pirwire.Create("seed", "items", relBytes(rowsRel("r",
			tcol("id", "int64", "value", "text"),
			[][]lirwire.Cell{{mustValue(1), mustValue("same")}, {mustValue(2), mustValue("same")}}))),
		pirwire.CreateIndex("unique_value", tableID, pirwire.IndexDefinition{
			Name: "items_value_key", Columns: []string{"value"}, Unique: ptrBool(true),
		}),
	))
	if err == nil {
		t.Fatal("unique-index backfill should reject preceding duplicate writes")
	}
	rows, queryErr := c.Query(ctx, scanOrdered("items", "i", "id"))
	tables, tableErr := c.Tables(ctx)
	info, infoErr := c.Info(ctx)
	if queryErr != nil || tableErr != nil || infoErr != nil || len(rows) != 0 ||
		len(tables) != 1 || len(tables[0].Indexes) != 0 || info.SchemaVersion != 1 {
		t.Fatalf("rollback left rows=%#v tables=%#v info=%+v errors=(%v, %v, %v)",
			rows, tables, info, queryErr, tableErr, infoErr)
	}
}

func TestExecuteOnlineIndexTransitionControl(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, valueColumn := pirwire.SchemaID(1), pirwire.SchemaID(1), pirwire.SchemaID(2)
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("schema", pirwire.TableDefinition{
		ID: &tableID, Name: "items",
		Columns: []pirwire.ColumnDefinition{
			{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
			{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
		},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}

	started, err := c.Execute(ctx, pirwire.Prog("", pirwire.StartIndexBuild("build", tableID, pirwire.IndexDefinition{
		Name: "items_value_online", Columns: []string{"value"},
	})))
	if err != nil {
		t.Fatal(err)
	}
	if len(started.Statements) != 1 || started.Statements[0].Control == nil {
		t.Fatalf("start response = %+v", started)
	}
	if started.Statements[0].Affected != 1 {
		t.Fatalf("start affected = %d, want one transition", started.Statements[0].Affected)
	}
	control := started.Statements[0].Control
	if control.Kind != "transition" || control.TransitionKind != "index_build" || control.State != "building" {
		t.Fatalf("start control = %+v", control)
	}

	inspected, err := c.SchemaTransition(ctx, control.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if inspected.TransitionID != control.TransitionID || inspected.State != radclient.TransitionBuilding {
		t.Fatalf("inspect control = %+v", inspected)
	}

	listed, err := c.SchemaTransitions(
		ctx,
		radclient.WithTransitionKind(radclient.TransitionIndexBuild),
		radclient.WithTransitionState(radclient.TransitionBuilding),
	)
	if err != nil {
		t.Fatal(err)
	}
	if len(listed) != 1 || listed[0].TransitionID != control.TransitionID {
		t.Fatalf("filtered transition list = %+v", listed)
	}

	cancelled, err := c.CancelSchemaTransition(ctx, control.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if cancelled.State != radclient.TransitionCancelled {
		t.Fatalf("cancel control = %+v", cancelled)
	}
	again, err := c.CancelSchemaTransition(ctx, control.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if again.State != radclient.TransitionCancelled || again.Generation != cancelled.Generation {
		t.Fatalf("idempotent cancel = %+v, first = %+v", again, cancelled)
	}
	if _, err := c.SchemaTransition(ctx, "tr-does-not-exist"); !radclient.IsNotFound(err) {
		t.Fatalf("missing inspect error = %v, want not_found", err)
	}
	if _, err := c.CancelSchemaTransition(ctx, "tr-does-not-exist"); !radclient.IsNotFound(err) {
		t.Fatalf("missing cancel error = %v, want not_found", err)
	}
}

func TestExecutePIRResolvesAfterToCommittedTransitionIdentity(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, valueColumn := pirwire.SchemaID(1), pirwire.SchemaID(1), pirwire.SchemaID(2)
	nullable := true
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("schema", pirwire.TableDefinition{
		ID: &tableID, Name: "items",
		Columns: []pirwire.ColumnDefinition{
			{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
			{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeInt64, Nullable: &nullable},
		},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}

	replacement := pirwire.StartColumnReplacement(
		"replace",
		tableID,
		valueColumn,
		pirwire.ColumnReplacementDefinition{Type: pirwire.ColumnTypeText, Nullable: true},
	)
	index := pirwire.StartIndexBuild(
		"build",
		tableID,
		pirwire.IndexDefinition{Name: "items_value_idx", Columns: []string{"value"}},
	)
	index = pirwire.WithAfter(index, "replace")
	result, err := c.Execute(ctx, pirwire.Prog("", replacement, index))
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Statements) != 2 || result.Statements[0].Control == nil || result.Statements[1].Control == nil {
		t.Fatalf("PIR transition summaries = %#v", result.Statements)
	}
	replacementControl := result.Statements[0].Control
	indexControl := result.Statements[1].Control
	if replacementControl.State != radclient.TransitionBuilding || indexControl.State != radclient.TransitionWaiting {
		t.Fatalf("replacement = %#v, index = %#v", replacementControl, indexControl)
	}
	if !reflect.DeepEqual(indexControl.Prerequisites, []string{replacementControl.TransitionID}) {
		t.Fatalf("PIR control after edge = %v, want [%s]", indexControl.Prerequisites, replacementControl.TransitionID)
	}
	stored, err := c.SchemaTransition(ctx, indexControl.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(stored.Prerequisites, []string{replacementControl.TransitionID}) {
		t.Fatalf("durable after edge = %v, want [%s]", stored.Prerequisites, replacementControl.TransitionID)
	}
}

func TestSchemaTransitionAdministrationIsAvailableInSchemaMode(t *testing.T) {
	c := testServerInMode(t, model.ModeSchema)
	ctx := context.Background()
	transitions, err := c.SchemaTransitions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(transitions) != 0 {
		t.Fatalf("fresh transition list = %+v, want empty", transitions)
	}
	if _, err := c.SchemaTransition(ctx, "tr-does-not-exist"); !radclient.IsNotFound(err) {
		t.Fatalf("missing inspect error = %v, want not_found", err)
	}
	if _, err := c.CancelSchemaTransition(ctx, "tr-does-not-exist"); !radclient.IsNotFound(err) {
		t.Fatalf("missing cancel error = %v, want not_found", err)
	}
}

func TestPIRLiteralDefaultKeepsFullInt64Precision(t *testing.T) {
	definition := pirwire.ColumnDefinition{
		Name: "counter", Type: pirwire.ColumnTypeInt64,
		Default: &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{
			Kind: "literal", Value: json.RawMessage(`9007199254740993`),
		}},
	}
	converted, err := pirColumnDef(definition)
	if err != nil {
		t.Fatal(err)
	}
	if converted.Default == nil || converted.Default.Int64 != 9007199254740993 {
		t.Fatalf("default = %+v", converted.Default)
	}
}

func TestPIRReplacementAndConstraintDefinitionsLowerWithoutSemanticLoss(t *testing.T) {
	conversion := "strict_builtin"
	format := "decimal"
	indexWire := pirwire.StartIndexBuild(
		"index",
		7,
		pirwire.IndexDefinition{Name: "values_online", Columns: []string{"value"}},
		"tr0",
		"tr1",
	)
	indexWire = pirwire.WithAfter(indexWire, "replace")
	index, err := statementToEngine(indexWire)
	if err != nil {
		t.Fatal(err)
	}
	if index.Kind != execprogram.StartIndexBuild ||
		index.TableID != 7 ||
		index.Index.Name != "values_online" ||
		!reflect.DeepEqual(index.Prerequisites, []string{"tr0", "tr1"}) ||
		!reflect.DeepEqual(index.After, []string{"replace"}) {
		t.Fatalf("index lowering = %+v", index)
	}

	replacementWire := pirwire.StartColumnReplacement(
		"replace",
		7,
		3,
		pirwire.ColumnReplacementDefinition{
			Type: pirwire.ColumnTypeInt64, Nullable: false,
			Format: &format, Conversion: &conversion,
			Prerequisites: []string{"tr1", "tr2"},
		},
	)
	replacementWire = pirwire.WithAfter(replacementWire, "prepare")
	replacement, err := statementToEngine(replacementWire)
	if err != nil {
		t.Fatal(err)
	}
	if replacement.Kind != execprogram.StartColumnReplacement ||
		replacement.TableID != 7 ||
		replacement.ColumnID != 3 ||
		replacement.Replacement.Type != model.TypeInt64 ||
		replacement.Replacement.Nullable ||
		replacement.Replacement.Format != format ||
		replacement.Replacement.Conversion != model.ColumnConversionStrictBuiltin ||
		!reflect.DeepEqual(replacement.Replacement.Prerequisites, []string{"tr1", "tr2"}) ||
		!reflect.DeepEqual(replacement.After, []string{"prepare"}) {
		t.Fatalf("replacement lowering = %+v", replacement)
	}

	constraintWire := pirwire.StartConstraintValidation(
		"validate",
		7,
		pirwire.ConstraintValidationDefinition{
			Name: "value_required", Kind: "not_null", ColumnID: 3,
			Prerequisites: []string{"tr2"},
		},
	)
	constraintWire = pirwire.WithAfter(constraintWire, "replace")
	constraint, err := statementToEngine(constraintWire)
	if err != nil {
		t.Fatal(err)
	}
	if constraint.Kind != execprogram.StartConstraintValidation ||
		constraint.Constraint.Name != "value_required" ||
		constraint.Constraint.Kind != model.ConstraintNotNull ||
		constraint.Constraint.ColumnID != 3 ||
		!reflect.DeepEqual(constraint.Constraint.Prerequisites, []string{"tr2"}) ||
		!reflect.DeepEqual(constraint.After, []string{"replace"}) {
		t.Fatalf("constraint lowering = %+v", constraint)
	}
}
