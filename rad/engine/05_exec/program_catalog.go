package exec

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func (tx *Tx) runCatalogStatement(ctx context.Context, change *change.Mutation, stmt execprogram.Statement) error {
	return tx.applyCatalogStatement(ctx, change, stmt, true)
}

func (tx *Tx) preflightCatalogStatement(ctx context.Context, change *change.Mutation, stmt execprogram.Statement) error {
	return tx.applyCatalogStatement(ctx, change, stmt, false)
}

func (tx *Tx) applyCatalogStatement(ctx context.Context, change *change.Mutation, stmt execprogram.Statement, backfill bool) error {
	switch stmt.Kind {
	case execprogram.CreateTable:
		_, err := change.CreateTable(ctx, stmt.TableDef)
		return err
	case execprogram.RenameTable:
		return change.RenameTableBySchemaID(ctx, stmt.TableID, stmt.To)
	case execprogram.DeleteTable:
		return change.DeleteTableBySchemaID(ctx, stmt.TableID)
	case execprogram.CreateColumn:
		_, err := change.CreateColumnBySchemaID(ctx, stmt.TableID, stmt.Column)
		return err
	case execprogram.RenameColumn:
		_, err := change.RenameColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID, stmt.To)
		return err
	case execprogram.DeleteColumn:
		_, err := change.DeleteColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID)
		return err
	case execprogram.CreateIndex:
		table, err := change.TableBySchemaID(ctx, stmt.TableID)
		if err != nil {
			return err
		}
		if backfill {
			return tx.CreateIndexWithBackfill(ctx, change, table.Name, stmt.Index)
		}
		_, _, err = change.CreateIndex(ctx, table.Name, stmt.Index)
		return err
	case execprogram.DeleteIndex:
		return change.DeleteIndexBySchemaID(ctx, stmt.TableID, stmt.IndexName)
	default:
		return reject.Inputf("exec: unknown catalog statement kind %q", stmt.Kind)
	}
}
