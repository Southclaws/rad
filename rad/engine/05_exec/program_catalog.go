package exec

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func (tx *Tx) runCatalogStatement(ctx context.Context, change *change.Mutation, stmt execprogram.Statement) (*model.TransitionControl, error) {
	control, err := tx.applyCatalogStatement(ctx, change, stmt, true)
	if err == nil && stmt.Kind.Catalog() {
		err = tx.markCatalogChanged()
	}
	return control, err
}

func (tx *Tx) preflightCatalogStatement(ctx context.Context, change *change.Mutation, stmt execprogram.Statement) (*model.TransitionControl, error) {
	control, err := tx.applyCatalogStatement(ctx, change, stmt, false)
	if err == nil && stmt.Kind.Catalog() {
		err = tx.markCatalogChanged()
	}
	return control, err
}

func (tx *Tx) applyCatalogStatement(ctx context.Context, change *change.Mutation, stmt execprogram.Statement, backfill bool) (*model.TransitionControl, error) {
	switch stmt.Kind {
	case execprogram.CreateTable:
		_, err := change.CreateTable(ctx, stmt.TableDef)
		return nil, err
	case execprogram.RenameTable:
		return nil, change.RenameTableBySchemaID(ctx, stmt.TableID, stmt.To)
	case execprogram.DeleteTable:
		return nil, change.DeleteTableBySchemaID(ctx, stmt.TableID)
	case execprogram.CreateColumn:
		_, err := change.CreateColumnBySchemaID(ctx, stmt.TableID, stmt.Column)
		return nil, err
	case execprogram.RenameColumn:
		_, err := change.RenameColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID, stmt.To)
		return nil, err
	case execprogram.ChangeColumnDefault:
		_, column, err := change.ColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID)
		if err != nil {
			return nil, err
		}
		var value *model.Default
		if stmt.InsertDefault != nil {
			value, err = stmt.InsertDefault.Resolve(column)
			if err != nil {
				return nil, reject.Inputf("catalog: %v", err)
			}
		}
		_, err = change.ChangeColumnInsertDefaultBySchemaID(
			ctx,
			stmt.TableID,
			stmt.ColumnID,
			value,
		)
		return nil, err
	case execprogram.DeleteColumn:
		_, err := change.DeleteColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID)
		return nil, err
	case execprogram.CreateIndex:
		table, err := change.TableBySchemaID(ctx, stmt.TableID)
		if err != nil {
			return nil, err
		}
		if backfill {
			return nil, tx.CreateIndexWithBackfill(ctx, change, table.Name, stmt.Index)
		}
		_, _, err = change.CreateIndex(ctx, table.Name, stmt.Index)
		return nil, err
	case execprogram.DeleteIndex:
		return nil, change.DeleteIndexBySchemaID(ctx, stmt.TableID, stmt.IndexName)
	case execprogram.StartIndexBuild:
		transition, err := change.StartIndexBuildWithLimitsAndPrerequisites(
			ctx,
			stmt.TableID,
			stmt.Index,
			stmt.Prerequisites,
			tx.e.schemaJobConfig.DeltaSoftLimit,
			tx.e.schemaJobConfig.DeltaHardLimit,
		)
		control := transition.Control()
		return &control, err
	case execprogram.StartColumnReplacement:
		transition, err := change.StartColumnReplacement(
			ctx,
			stmt.TableID,
			stmt.ColumnID,
			stmt.Replacement,
		)
		control := transition.Control()
		return &control, err
	case execprogram.StartConstraintValidation:
		transition, err := change.StartConstraintValidation(ctx, stmt.TableID, stmt.Constraint)
		control := transition.Control()
		return &control, err
	default:
		return nil, reject.Inputf("exec: unknown catalog statement kind %q", stmt.Kind)
	}
}
