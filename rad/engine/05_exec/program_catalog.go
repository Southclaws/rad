package exec

import (
	"context"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// CatalogPolicy authorises catalog statements and selects their schema
// revision boundary. It is execution policy supplied by the caller, not part
// of PIR syntax.
type CatalogPolicy uint8

const (
	// CatalogForbidden is the zero value so existing relational callers do not
	// acquire catalog authority accidentally.
	CatalogForbidden CatalogPolicy = iota
	// CatalogRevisionPerStatement records every catalog statement as a separate
	// revision while the complete Program still commits atomically.
	CatalogRevisionPerStatement
	// CatalogRevisionPerProgram records the final schema left by the complete
	// Program as one revision.
	CatalogRevisionPerProgram
)

func (tx *Tx) runCatalogStatement(ctx context.Context, change *catalog.Mutation, stmt ProgramStatement) error {
	return tx.applyCatalogStatement(ctx, change, stmt, true)
}

func (tx *Tx) preflightCatalogStatement(ctx context.Context, change *catalog.Mutation, stmt ProgramStatement) error {
	return tx.applyCatalogStatement(ctx, change, stmt, false)
}

func (tx *Tx) applyCatalogStatement(ctx context.Context, change *catalog.Mutation, stmt ProgramStatement, backfill bool) error {
	switch stmt.Kind {
	case StmtCreateTable:
		_, err := change.CreateTable(ctx, stmt.TableDef)
		return err
	case StmtRenameTable:
		return change.RenameTableBySchemaID(ctx, stmt.TableID, stmt.To)
	case StmtDeleteTable:
		return change.DeleteTableBySchemaID(ctx, stmt.TableID)
	case StmtCreateColumn:
		_, err := change.CreateColumnBySchemaID(ctx, stmt.TableID, stmt.Column)
		return err
	case StmtRenameColumn:
		_, err := change.RenameColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID, stmt.To)
		return err
	case StmtDeleteColumn:
		_, err := change.DeleteColumnBySchemaID(ctx, stmt.TableID, stmt.ColumnID)
		return err
	case StmtCreateIndex:
		table, err := change.TableBySchemaID(ctx, stmt.TableID)
		if err != nil {
			return err
		}
		if backfill {
			return tx.CreateIndexWithBackfill(ctx, change, table.Name, stmt.Index)
		}
		_, _, err = change.CreateIndex(ctx, table.Name, stmt.Index)
		return err
	case StmtDeleteIndex:
		return change.DeleteIndexBySchemaID(ctx, stmt.TableID, stmt.IndexName)
	default:
		return reject.Inputf("exec: unknown catalog statement kind %q", stmt.Kind)
	}
}
