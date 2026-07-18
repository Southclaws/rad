package api

// The imperative catalog operations: create/update/delete for tables,
// columns, and indexes. These are the direct-mode mutation channel; they
// drive exactly the same frontend catalog façade the schema.rad reconciler
// uses, so the two channels cannot diverge semantically. The only extra
// behaviour here is the mode gate: a schema-managed database rejects every
// operation in this group before touching the catalog.

import (
	"context"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/protocol"
)

func (a *dbAPI) TableCreate(ctx context.Context, req oas.OptTableDef) (oas.TableCreateRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.TableCreateUnprocessableEntity)(&p), nil
	}
	def, err := catTableDef(api.TableDefFromOAS(req.Or(oas.TableDef{})))
	if err == nil {
		var tbl catalog.Table
		if tbl, err = a.db.CreateTable(ctx, def); err == nil {
			return a.tableOAS(ctx, tbl)
		}
	}
	if p := catalogProblem(err); p != nil {
		if p.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.TableCreateConflict)(p), nil
		}
		return (*oas.TableCreateUnprocessableEntity)(p), nil
	}
	return nil, err
}

func (a *dbAPI) TableDelete(ctx context.Context, params oas.TableDeleteParams) (oas.TableDeleteRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.TableDeleteUnprocessableEntity)(&p), nil
	}
	err := a.db.DeleteTable(ctx, params.Table)
	if err == nil {
		return &oas.NoContent{}, nil
	}
	if p := catalogProblem(err); p != nil {
		if p.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.TableDeleteConflict)(p), nil
		}
		return (*oas.TableDeleteUnprocessableEntity)(p), nil
	}
	return nil, err
}

func (a *dbAPI) TableUpdate(ctx context.Context, req oas.OptTableUpdateProps, params oas.TableUpdateParams) (oas.TableUpdateRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.TableUpdateUnprocessableEntity)(&p), nil
	}
	name := req.Or(oas.TableUpdateProps{}).Name
	err := a.db.RenameTable(ctx, params.Table, name)
	if err == nil {
		tbl, ok, gerr := a.cat.GetTable(ctx, name)
		if gerr != nil {
			return nil, gerr
		}
		if !ok {
			err = wireErrf("table %q does not exist", name)
		} else {
			return a.tableOAS(ctx, tbl)
		}
	}
	if op := catalogProblem(err); op != nil {
		if op.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.TableUpdateConflict)(op), nil
		}
		return (*oas.TableUpdateUnprocessableEntity)(op), nil
	}
	return nil, err
}

func (a *dbAPI) ColumnCreate(ctx context.Context, req oas.OptColumnDef, params oas.ColumnCreateParams) (oas.ColumnCreateRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.ColumnCreateUnprocessableEntity)(&p), nil
	}
	def, err := catColumnDef(api.ColumnDefFromOAS(req.Or(oas.ColumnDef{})))
	if err == nil {
		var tbl catalog.Table
		if tbl, err = a.db.CreateColumn(ctx, params.Table, def); err == nil {
			return a.tableOAS(ctx, tbl)
		}
	}
	if op := catalogProblem(err); op != nil {
		if op.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.ColumnCreateConflict)(op), nil
		}
		return (*oas.ColumnCreateUnprocessableEntity)(op), nil
	}
	return nil, err
}

func (a *dbAPI) ColumnDelete(ctx context.Context, params oas.ColumnDeleteParams) (oas.ColumnDeleteRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.ColumnDeleteUnprocessableEntity)(&p), nil
	}
	tbl, err := a.db.DeleteColumn(ctx, params.Table, params.Column)
	if err == nil {
		return a.tableOAS(ctx, tbl)
	}
	if op := catalogProblem(err); op != nil {
		if op.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.ColumnDeleteConflict)(op), nil
		}
		return (*oas.ColumnDeleteUnprocessableEntity)(op), nil
	}
	return nil, err
}

func (a *dbAPI) ColumnUpdate(ctx context.Context, req oas.OptColumnUpdateProps, params oas.ColumnUpdateParams) (oas.ColumnUpdateRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.ColumnUpdateUnprocessableEntity)(&p), nil
	}
	tbl, err := a.db.RenameColumn(ctx, params.Table, params.Column, req.Or(oas.ColumnUpdateProps{}).Name)
	if err == nil {
		return a.tableOAS(ctx, tbl)
	}
	if op := catalogProblem(err); op != nil {
		if op.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.ColumnUpdateConflict)(op), nil
		}
		return (*oas.ColumnUpdateUnprocessableEntity)(op), nil
	}
	return nil, err
}

func (a *dbAPI) IndexCreate(ctx context.Context, req oas.OptIndexInfo, params oas.IndexCreateParams) (oas.IndexCreateRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.IndexCreateUnprocessableEntity)(&p), nil
	}
	idx := api.IndexFromOAS(req.Or(oas.IndexInfo{}))
	err := a.db.CreateIndex(ctx, params.Table, catalog.IndexDef{Name: idx.Name, Columns: idx.Columns, Unique: idx.Unique})
	if err == nil {
		tbl, ok, gerr := a.cat.GetTable(ctx, params.Table)
		if gerr != nil {
			return nil, gerr
		}
		if !ok {
			err = wireErrf("table %q does not exist", params.Table)
		} else {
			return a.tableOAS(ctx, tbl)
		}
	}
	if op := catalogProblem(err); op != nil {
		if op.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.IndexCreateConflict)(op), nil
		}
		return (*oas.IndexCreateUnprocessableEntity)(op), nil
	}
	return nil, err
}

func (a *dbAPI) IndexDelete(ctx context.Context, params oas.IndexDeleteParams) (oas.IndexDeleteRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.IndexDeleteUnprocessableEntity)(&p), nil
	}
	err := a.db.DeleteIndex(ctx, params.Table, params.Index)
	if err == nil {
		tbl, ok, gerr := a.cat.GetTable(ctx, params.Table)
		if gerr != nil {
			return nil, gerr
		}
		if !ok {
			err = wireErrf("table %q does not exist", params.Table)
		} else {
			return a.tableOAS(ctx, tbl)
		}
	}
	if op := catalogProblem(err); op != nil {
		if op.Code == oas.ProblemCode(protocol.CodeConflict) {
			return (*oas.IndexDeleteConflict)(op), nil
		}
		return (*oas.IndexDeleteUnprocessableEntity)(op), nil
	}
	return nil, err
}
