package migration

import (
	"slices"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

func TestMigrationProgramBuildsDependenciesFromStableColumnIdentity(t *testing.T) {
	current := []model.Table{{
		SchemaID: 41,
		Name:     "events",
		Columns: []model.Column{
			{SchemaID: 1, Name: "id", Type: model.TypeInt64},
			{SchemaID: 2, Name: "left_value", Type: model.TypeText},
			{SchemaID: 3, Name: "right_value", Type: model.TypeText},
		},
	}}
	steps := []migrate.Step{
		migrate.RenameTable{From: "events", To: "measurements"},
		migrate.RenameColumn{Table: "measurements", From: "left_value", To: "minimum"},
		migrate.ReplaceColumn{
			Table: "measurements", Column: "minimum", ColumnID: 2,
			Def: model.ColumnReplacementDef{Type: model.TypeInt64, Conversion: model.ColumnConversionStrictBuiltin},
		},
		migrate.ReplaceColumn{
			Table: "measurements", Column: "right_value", ColumnID: 3,
			Def: model.ColumnReplacementDef{Type: model.TypeInt64, Conversion: model.ColumnConversionStrictBuiltin},
		},
		migrate.ValidateNotNull{
			Table: "measurements", Column: "minimum",
			Def: model.ConstraintDef{Name: "measurements_minimum_not_null", Kind: model.ConstraintNotNull, ColumnID: 2},
		},
		migrate.CreateIndex{
			Table: "measurements",
			Def:   model.IndexDef{Name: "measurements_range_uq", Columns: []string{"minimum", "right_value", "minimum"}, Unique: true},
		},
	}

	program, err := migrationProgram(current, steps)
	if err != nil {
		t.Fatal(err)
	}
	if len(program.Statements) != len(steps) {
		t.Fatalf("statements = %#v", program.Statements)
	}
	for _, statement := range program.Statements {
		if statement.Kind != execprogram.RenameTable && statement.Kind != execprogram.CreateTable && statement.TableID != 41 {
			t.Fatalf("statement %q resolved table ID %d, want stable ID 41", statement.Name, statement.TableID)
		}
	}
	if got := program.Statements[2]; got.Kind != execprogram.StartColumnReplacement || got.ColumnID != 2 {
		t.Fatalf("first replacement = %#v", got)
	}
	if got := program.Statements[4]; got.Kind != execprogram.StartConstraintValidation || !slices.Equal(got.After, []string{"migration_3"}) {
		t.Fatalf("constraint dependency = %#v", got)
	}
	if got := program.Statements[5]; got.Kind != execprogram.StartIndexBuild ||
		!slices.Equal(got.After, []string{"migration_3", "migration_4"}) {
		t.Fatalf("deduplicated index dependencies = %#v", got)
	}
}

func TestMigrationProgramDoesNotLeakDependenciesAcrossTables(t *testing.T) {
	current := []model.Table{
		{SchemaID: 1, Name: "left", Columns: []model.Column{{SchemaID: 1, Name: "value", Type: model.TypeText}}},
		{SchemaID: 2, Name: "right", Columns: []model.Column{{SchemaID: 1, Name: "value", Type: model.TypeText}}},
	}
	program, err := migrationProgram(current, []migrate.Step{
		migrate.ReplaceColumn{
			Table: "left", Column: "value", ColumnID: 1,
			Def: model.ColumnReplacementDef{Type: model.TypeInt64, Conversion: model.ColumnConversionStrictBuiltin},
		},
		migrate.CreateIndex{Table: "right", Def: model.IndexDef{Name: "right_value_idx", Columns: []string{"value"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if got := program.Statements[1].After; len(got) != 0 {
		t.Fatalf("unrelated index inherited replacement dependency %v", got)
	}
}
