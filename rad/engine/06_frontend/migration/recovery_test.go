package migration

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func TestRecoverMigrationTransitionsMatchesExactRequestAndIgnoresTerminalHistory(t *testing.T) {
	desired := model.Schema{Tables: []model.TableDef{{
		ID: 7, Name: "events",
		Columns: []model.ColumnDef{
			{ID: 1, Name: "id", Type: model.TypeInt64},
			{ID: 2, Name: "value", Type: model.TypeInt64, Nullable: true},
		},
	}}}
	replacement := migrate.ReplaceColumn{
		Table: "events", Column: "value", ColumnID: 2,
		Def: model.ColumnReplacementDef{
			Type: model.TypeInt64, Nullable: true, Conversion: model.ColumnConversionStrictBuiltin,
		},
	}
	exactRequest := &model.ColumnReplacementRequest{
		ColumnSchemaID: 2, Type: model.TypeInt64, Nullable: true,
		Conversion: model.ColumnConversionStrictBuiltin,
	}

	t.Run("active exact request is recovered", func(t *testing.T) {
		program, recovered, blocking := recoverMigrationTransitions(desired, []migrate.Step{replacement}, []model.SchemaTransition{{
			ID: "active", Kind: model.TransitionColumnReplacement, State: model.TransitionBuilding,
			TableSchemaID: 7, AffectedColumnIDs: []model.SchemaID{2}, ReplacementRequest: exactRequest,
		}})
		if len(program) != 0 || len(blocking) != 0 || len(recovered) != 1 || recovered[0].TransitionID != "active" {
			t.Fatalf("program=%#v recovered=%#v blocking=%#v", program, recovered, blocking)
		}
	})

	t.Run("terminal exact request does not suppress fresh work", func(t *testing.T) {
		for _, state := range []model.TransitionState{
			model.TransitionReady, model.TransitionFailed, model.TransitionCancelled,
		} {
			t.Run(string(state), func(t *testing.T) {
				program, recovered, blocking := recoverMigrationTransitions(desired, []migrate.Step{replacement}, []model.SchemaTransition{{
					ID: "terminal", Kind: model.TransitionColumnReplacement, State: state,
					TableSchemaID: 7, AffectedColumnIDs: []model.SchemaID{2}, ReplacementRequest: exactRequest,
				}})
				if len(program) != 1 || len(recovered) != 0 || len(blocking) != 0 {
					t.Fatalf("program=%#v recovered=%#v blocking=%#v", program, recovered, blocking)
				}
			})
		}
	})

	t.Run("active different request blocks instead of being reused", func(t *testing.T) {
		program, recovered, blocking := recoverMigrationTransitions(desired, []migrate.Step{replacement}, []model.SchemaTransition{{
			ID: "conflict", Kind: model.TransitionColumnReplacement, State: model.TransitionBuilding,
			TableSchemaID: 7, AffectedColumnIDs: []model.SchemaID{2},
			ReplacementRequest: &model.ColumnReplacementRequest{
				ColumnSchemaID: 2, Type: model.TypeBool, Nullable: true,
				Conversion: model.ColumnConversionStrictBuiltin,
			},
		}})
		if len(program) != 0 || len(recovered) != 0 || len(blocking) != 1 || blocking[0].Kind != "active_schema_transition_conflict" {
			t.Fatalf("program=%#v recovered=%#v blocking=%#v", program, recovered, blocking)
		}
	})
}
