package exec

import (
	"context"
	"strings"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

func TestTransactionExecutesMultiplePIRProgramsWithoutChangingPIR(t *testing.T) {
	engine, ctx := setup(t)
	tx, err := engine.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	insert := execprogram.Program{Statements: []execprogram.Statement{{
		Name: "insert", Kind: execprogram.Create, Table: "users",
		Rel: lir.Query{Card: lir.CardMany, Root: lir.Rows{
			Scope: "r",
			Columns: []lir.RowsCol{
				{Name: "id", Kind: lir.KindInt64},
				{Name: "name", Kind: lir.KindText},
			},
			Values: [][]any{{int64(77), "inside"}},
		}},
	}}}
	if _, err := tx.ExecuteProgram(ctx, insert, execprogram.Options{}); err != nil {
		t.Fatal(err)
	}

	query := execprogram.Program{Statements: []execprogram.Statement{{
		Name: "read", Kind: execprogram.Query,
		Rel: lir.Query{Card: lir.CardMany, Root: lir.Order{
			Input: lir.Scan{Table: "users", Scope: "u"},
			Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "u", Name: "id"}}},
		}},
	}}}
	result, err := tx.ExecuteProgram(ctx, query, execprogram.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if result.Result.Kind != lir.DatumArray || len(result.Result.Elems) != 1 {
		t.Fatalf("transaction query result = %#v, want own uncommitted row", result.Result)
	}
	if _, ok, err := engine.GetByPrimaryKey(context.Background(), "users", lir.Row{"id": lir.Int64(77)}); err != nil || ok {
		t.Fatalf("uncommitted row leaked: ok=%v err=%v", ok, err)
	}

	if err := tx.Rollback(); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := engine.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(77)}); err != nil || ok {
		t.Fatalf("rolled-back row visible: ok=%v err=%v", ok, err)
	}
}

func TestExecuteProgramRejectsStaleExpectedCatalog(t *testing.T) {
	engine, ctx := setup(t)
	expected, err := engine.Catalog().Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := engine.Catalog().CreateTable(ctx, model.TableDef{
		Name: "newer",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}

	_, err = engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "value", Kind: execprogram.Query,
		Rel: lir.Query{Card: lir.CardFirst, Root: lir.Rows{
			Scope: "r", Columns: []lir.RowsCol{{Name: "n", Kind: lir.KindInt64}}, Values: [][]any{{1}},
		}},
	}}}, execprogram.Options{ExpectedCatalog: &expected})
	if !IsConflict(err) {
		t.Fatalf("error = %v, want retryable catalog conflict", err)
	}
}

func TestExecuteProgramStartsTransitionControl(t *testing.T) {
	engine, ctx := setup(t)
	table, ok, err := engine.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("table: ok=%v err=%v", ok, err)
	}
	start, err := engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "build", Kind: execprogram.StartIndexBuild, TableID: table.SchemaID,
		Index: model.IndexDef{Name: "users_age_online", Columns: []string{"age"}},
	}}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerStatement})
	if err != nil {
		t.Fatal(err)
	}
	if len(start.Statements) != 1 || start.Statements[0].Control == nil {
		t.Fatalf("start summary = %+v", start.Statements)
	}
	if start.Statements[0].Affected != 1 {
		t.Fatalf("start affected = %d, want one transition", start.Statements[0].Affected)
	}
	control := start.Statements[0].Control
	if control.Kind != "transition" || control.TransitionKind != model.TransitionIndexBuild ||
		control.State != model.TransitionBuilding || control.TransitionID == "" || control.ObjectID == "" {
		t.Fatalf("start control = %+v", control)
	}

	inspect, err := engine.inspectSchemaTransition(ctx, control.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if got := inspect.Control(); got.TransitionID != control.TransitionID || got.State != model.TransitionBuilding {
		t.Fatalf("inspect control = %+v", got)
	}

	cancel, err := engine.CancelSchemaTransition(ctx, control.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if got := cancel.Control(); got.State != model.TransitionCancelled {
		t.Fatalf("cancel control = %+v", got)
	}
}

func TestExecuteProgramPublishesConfiguredIndexDeltaLimits(t *testing.T) {
	const softLimit, hardLimit = uint64(3), uint64(5)
	engine, ctx := setupWithOptions(
		t,
		WithSchemaJobScheduling(false),
		WithSchemaJobConfig(SchemaJobConfig{
			DeltaSoftLimit: softLimit,
			DeltaHardLimit: hardLimit,
		}),
	)
	table, ok, err := engine.Catalog().GetTable(ctx, "users")
	if err != nil || !ok {
		t.Fatalf("table: ok=%v err=%v", ok, err)
	}
	result, err := engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "build", Kind: execprogram.StartIndexBuild, TableID: table.SchemaID,
		Index: model.IndexDef{Name: "users_age_limited", Columns: []string{"age"}},
	}}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerStatement})
	if err != nil {
		t.Fatal(err)
	}
	control := result.Statements[0].Control
	if control == nil {
		t.Fatalf("start summary = %+v", result.Statements)
	}
	transition, err := engine.inspectSchemaTransition(ctx, control.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if transition.DeltaSoftLimit != softLimit || transition.DeltaHardLimit != hardLimit {
		t.Fatalf(
			"PIR transition delta limits = %d/%d, want %d/%d",
			transition.DeltaSoftLimit,
			transition.DeltaHardLimit,
			softLimit,
			hardLimit,
		)
	}
	txn, err := engine.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	defer txn.Rollback()
	storedTable, ok, err := store.New(txn).GetTableByID(ctx, transition.TableID)
	if err != nil || !ok {
		t.Fatalf("stored table: ok=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, txn, storedTable)
	if err != nil {
		t.Fatal(err)
	}
	if len(protocol.DeltaSinks) != 1 ||
		protocol.DeltaSinks[0].TransitionID != transition.ID ||
		protocol.DeltaSinks[0].DeltaHardLimit != hardLimit {
		t.Fatalf("PIR write-protocol delta sink = %+v", protocol.DeltaSinks)
	}
}

func TestExecuteProgramStartsReplacementAndConstraintTransitions(t *testing.T) {
	engine, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	table, column := replacementColumn(t, ctx, engine, "users", "age")
	replacement, err := engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "replace", Kind: execprogram.StartColumnReplacement,
		TableID: table.SchemaID, ColumnID: column.SchemaID,
		Replacement: model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	}}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerStatement})
	if err != nil {
		t.Fatal(err)
	}
	replacementControl := replacement.Statements[0].Control
	if replacement.Statements[0].Affected != 1 || replacementControl == nil ||
		replacementControl.TransitionKind != model.TransitionColumnReplacement ||
		replacementControl.State != model.TransitionBuilding {
		t.Fatalf("replacement PIR result = %+v", replacement.Statements)
	}
	if _, err := engine.CancelSchemaTransition(ctx, replacementControl.TransitionID); err != nil {
		t.Fatal(err)
	}

	constraint, err := engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "validate", Kind: execprogram.StartConstraintValidation,
		TableID: table.SchemaID,
		Constraint: model.ConstraintDef{
			Name: "users_age_required", Kind: model.ConstraintNotNull, ColumnID: column.SchemaID,
		},
	}}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerStatement})
	if err != nil {
		t.Fatal(err)
	}
	constraintControl := constraint.Statements[0].Control
	if constraint.Statements[0].Affected != 1 || constraintControl == nil ||
		constraintControl.TransitionKind != model.TransitionConstraintValidation ||
		constraintControl.State != model.TransitionBuilding {
		t.Fatalf("constraint PIR result = %+v", constraint.Statements)
	}
}

func TestExecuteProgramResolvesEarlierTransitionCompletionDependencies(t *testing.T) {
	engine, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	table, column := replacementColumn(t, ctx, engine, "users", "age")

	result, err := engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{
		{
			Name: "replace", Kind: execprogram.StartColumnReplacement,
			TableID: table.SchemaID, ColumnID: column.SchemaID,
			Replacement: model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
		},
		{
			Name: "build", Kind: execprogram.StartIndexBuild,
			TableID: table.SchemaID,
			Index:   model.IndexDef{Name: "users_age_after_replacement", Columns: []string{"age"}},
			After:   []string{"replace"},
		},
	}}, execprogram.Options{Catalog: execprogram.CatalogRevisionPerProgram})
	if err != nil {
		t.Fatal(err)
	}
	if len(result.Statements) != 2 || result.Statements[0].Control == nil || result.Statements[1].Control == nil {
		t.Fatalf("transition summaries = %+v", result.Statements)
	}
	replacement := result.Statements[0].Control
	index := result.Statements[1].Control
	if replacement.State != model.TransitionBuilding || index.State != model.TransitionWaiting {
		t.Fatalf("replacement = %+v, index = %+v", replacement, index)
	}
	stored, err := engine.inspectSchemaTransition(ctx, index.TransitionID)
	if err != nil {
		t.Fatal(err)
	}
	if len(stored.Prerequisites) != 1 || stored.Prerequisites[0] != replacement.TransitionID {
		t.Fatalf("index prerequisites = %v, want [%s]", stored.Prerequisites, replacement.TransitionID)
	}
}

func TestExecuteProgramRejectsInvalidTransitionCompletionDependenciesAtomically(t *testing.T) {
	tests := []struct {
		name       string
		statements []execprogram.Statement
		contains   string
	}{
		{
			name: "forward reference",
			statements: []execprogram.Statement{
				{Name: "build", Kind: execprogram.StartIndexBuild, After: []string{"replace"}},
				{Name: "replace", Kind: execprogram.StartColumnReplacement},
			},
			contains: "not an earlier transition start",
		},
		{
			name: "self reference",
			statements: []execprogram.Statement{
				{Name: "replace", Kind: execprogram.StartColumnReplacement, After: []string{"replace"}},
			},
			contains: "not an earlier transition start",
		},
		{
			name: "ordinary catalog statement",
			statements: []execprogram.Statement{
				{Name: "rename", Kind: execprogram.RenameTable},
				{Name: "build", Kind: execprogram.StartIndexBuild, After: []string{"rename"}},
			},
			contains: "not an earlier transition start",
		},
		{
			name: "after on synchronous statement",
			statements: []execprogram.Statement{
				{Name: "replace", Kind: execprogram.StartColumnReplacement},
				{Name: "rename", Kind: execprogram.RenameTable, After: []string{"replace"}},
			},
			contains: "cannot wait for a transition statement",
		},
		{
			name: "duplicate reference",
			statements: []execprogram.Statement{
				{Name: "replace", Kind: execprogram.StartColumnReplacement},
				{Name: "build", Kind: execprogram.StartIndexBuild, After: []string{"replace", "replace"}},
			},
			contains: "duplicate after reference",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			engine, ctx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
			table, column := replacementColumn(t, ctx, engine, "users", "age")
			for i := range tt.statements {
				statement := &tt.statements[i]
				if statement.TableID == 0 {
					statement.TableID = table.SchemaID
				}
				if statement.Kind == execprogram.StartColumnReplacement {
					statement.ColumnID = column.SchemaID
					statement.Replacement = model.ColumnReplacementDef{Type: model.TypeText, Nullable: true}
				}
				if statement.Kind == execprogram.StartIndexBuild {
					statement.Index = model.IndexDef{Name: "users_age_after_replacement", Columns: []string{"age"}}
				}
				if statement.Kind == execprogram.RenameTable {
					statement.To = "renamed_users"
				}
			}

			before, err := engine.Catalog().ListTransitions(ctx)
			if err != nil {
				t.Fatal(err)
			}
			_, err = engine.ExecuteProgram(ctx, execprogram.Program{Statements: tt.statements}, execprogram.Options{
				Catalog: execprogram.CatalogRevisionPerProgram,
			})
			if err == nil || !strings.Contains(err.Error(), tt.contains) {
				t.Fatalf("error = %v, want containing %q", err, tt.contains)
			}
			after, listErr := engine.Catalog().ListTransitions(ctx)
			if listErr != nil {
				t.Fatal(listErr)
			}
			if len(after) != len(before) {
				t.Fatalf("failed program persisted transitions: before=%d after=%d", len(before), len(after))
			}
		})
	}
}
