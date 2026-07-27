package concurrent

import (
	"context"
	"fmt"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func (w *workload) addColumnAction(round, actor int) action {
	columnID := pirwire.SchemaID(10_000 + round*w.scenario.MetadataAdds + actor)
	columnName := fmt.Sprintf("extra_r%03d_a%02d", round, actor)
	nullable := true
	a := action{
		Round: round, Actor: fmt.Sprintf("catalog-add-%02d", actor), Kind: "add_nullable_column",
		Detail: map[string]any{"column": columnName, "column_id": columnID},
	}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		_, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, radclient.IsConflict, func() (radclient.ProgramResult, error) {
			return w.db.Control.Execute(ctx, pirwire.Prog("", pirwire.CreateColumn(
				"add", probeTableID, pirwire.ColumnDefinition{
					ID: &columnID, Name: columnName, Type: pirwire.ColumnTypeText, Nullable: &nullable,
				},
			)))
		})
		return map[string]any{"column": columnName}, err
	}
	return a
}

func (w *workload) renameTableAction(round int) action {
	name := fmt.Sprintf("catalog_probe_r%03d", round)
	a := action{Round: round, Actor: "catalog-table-renamer", Kind: "rename_table", Detail: map[string]any{"to": name}}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		_, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, radclient.IsConflict, func() (radclient.ProgramResult, error) {
			return w.db.Control.Execute(ctx, pirwire.Prog("", pirwire.RenameTable("rename", probeTableID, name)))
		})
		return map[string]any{"name": name}, err
	}
	return a
}

func (w *workload) renameColumnAction(round int) action {
	name := fmt.Sprintf("payload_r%03d", round)
	a := action{Round: round, Actor: "catalog-column-renamer", Kind: "rename_column", Detail: map[string]any{"to": name}}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		_, err := retry(ctx, w.journal, a, w.scenario.MaxRetries, radclient.IsConflict, func() (radclient.ProgramResult, error) {
			return w.db.Control.Execute(ctx, pirwire.Prog("", pirwire.RenameColumn(
				"rename", probeTableID, probePayloadColumn, name,
			)))
		})
		return map[string]any{"name": name}, err
	}
	return a
}

func (w *workload) indexSchedulerAction(round int) action {
	a := action{Round: round, Actor: "schema-scheduler-observer", Kind: "index_build_progress"}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		transition, err := w.db.Control.SchemaTransition(ctx, w.transitionID)
		if err != nil {
			return nil, err
		}
		return map[string]any{
			"state": transition.State, "rows_scanned": transition.RowsScanned,
			"applied_delta": transition.AppliedDelta,
		}, nil
	}
	return a
}

func (w *workload) inspectTransitionAction(round int) action {
	a := action{Round: round, Actor: "transition-observer", Kind: "schema_transition_inspect"}
	a.Run = func(ctx context.Context) (map[string]any, error) {
		control, err := w.db.Control.SchemaTransition(ctx, w.transitionID)
		if err != nil {
			return nil, err
		}
		if control.Generation < w.lastProgress {
			return nil, fmt.Errorf("transition generation moved backwards: %d -> %d", w.lastProgress, control.Generation)
		}
		w.lastProgress = control.Generation
		return map[string]any{
			"state": control.State, "generation": control.Generation,
			"rows_scanned": control.RowsScanned, "delta_lag": control.DeltaLag,
		}, nil
	}
	return a
}
