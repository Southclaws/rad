package api

// The imperative catalog operations: create/update/delete for tables,
// columns, and indexes. These are the direct-mode mutation channel; they
// drive exactly the same frontend catalog façade the schema.rad reconciler
// uses, so the two channels cannot diverge semantically. The only extra
// behaviour here is the mode gate: a schema-managed database rejects every
// operation in this group before touching the catalog.

import (
	"context"
	"encoding/json"
	"net/http"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/protocol"
)

// schemaManagedProblem is the uniform rejection for every imperative catalog
// operation on a schema-managed database. Retrying cannot help — the mode is
// set once — so this is an invalid request, not a conflict.
func schemaManagedProblem() oas.Problem {
	return api.ProblemToOAS(protocol.NewProblem(
		protocol.CodeInvalid, http.StatusUnprocessableEntity,
		"catalog: this database is schema-managed; direct catalog changes are disabled — apply changes through schema migration",
	))
}

// catalogProblem classifies a catalog mutation error as a client problem, or
// returns nil for an internal error.
func catalogProblem(err error) *oas.Problem {
	if p := clientProblem(err); p != nil {
		op := api.ProblemToOAS(*p)
		return &op
	}
	return nil
}

// -
// wire definitions -> engine definitions
// -

func catTableDef(d protocol.TableDef) (catalog.TableDef, error) {
	def := catalog.TableDef{Name: d.Name, PrimaryKey: d.PrimaryKey}
	for _, c := range d.Columns {
		cd, err := catColumnDef(c)
		if err != nil {
			return catalog.TableDef{}, err
		}
		def.Columns = append(def.Columns, cd)
	}
	for _, i := range d.Indexes {
		def.Indexes = append(def.Indexes, catalog.IndexDef{Name: i.Name, Columns: i.Columns, Unique: i.Unique})
	}
	for _, fk := range d.ForeignKeys {
		def.ForeignKeys = append(def.ForeignKeys, catalog.ForeignKeyDef{
			Name: fk.Name, Columns: fk.Columns, RefTable: fk.RefTable, RefColumns: fk.RefColumns,
		})
	}
	return def, nil
}

func catColumnDef(c protocol.ColumnDef) (catalog.ColumnDef, error) {
	def := catalog.ColumnDef{
		Name: c.Name, Type: catalog.Type(c.Type), Nullable: c.Nullable, Format: c.Format,
	}
	d, err := catDefault(c.Name, def.Type, c.Default)
	if err != nil {
		return catalog.ColumnDef{}, err
	}
	def.Default = d
	return def, nil
}

// catDefault coerces a wire column default into the engine's typed form.
// Generator validity (uuid on text, now_ms on int64) is the catalog's rule;
// literal values are coerced here because only the wire layer knows the
// incoming JSON representation.
func catDefault(colName string, typ catalog.Type, in *protocol.ColumnDefault) (*catalog.Default, error) {
	if in == nil {
		return nil, nil
	}
	if in.Func != "" && in.Value != nil {
		return nil, wireErrf("column %q: default sets both func and value", colName)
	}
	if in.Func != "" {
		return &catalog.Default{Func: catalog.DefaultFunc(in.Func)}, nil
	}
	if in.Value == nil {
		return nil, wireErrf("column %q: default must set func or value", colName)
	}
	switch typ {
	case catalog.TypeText:
		s, ok := in.Value.(string)
		if !ok {
			return nil, wireErrf("column %q: default expects a string, got %T", colName, in.Value)
		}
		return &catalog.Default{Text: s}, nil
	case catalog.TypeInt64:
		n, ok := in.Value.(json.Number)
		if !ok {
			return nil, wireErrf("column %q: default expects a number, got %T", colName, in.Value)
		}
		i, err := n.Int64()
		if err != nil {
			return nil, wireErrf("column %q: default expects an integer, got %v", colName, n)
		}
		return &catalog.Default{Int64: i}, nil
	case catalog.TypeFloat64:
		n, ok := in.Value.(json.Number)
		if !ok {
			return nil, wireErrf("column %q: default expects a number, got %T", colName, in.Value)
		}
		f, err := n.Float64()
		if err != nil {
			return nil, wireErrf("column %q: default expects a float, got %v", colName, n)
		}
		return &catalog.Default{Float64: f}, nil
	case catalog.TypeBool:
		b, ok := in.Value.(bool)
		if !ok {
			return nil, wireErrf("column %q: default expects a boolean, got %T", colName, in.Value)
		}
		return &catalog.Default{Bool: b}, nil
	}
	return nil, wireErrf("column %q has unsupported type %q", colName, typ)
}

// -
// engine tables -> wire introspection
// -

// defaultInfo renders a column default for introspection; the column's type
// selects which literal field carries the value.
func defaultInfo(c catalog.Column) *protocol.ColumnDefault {
	d := c.Default
	if d == nil {
		return nil
	}
	if d.Func != "" {
		return &protocol.ColumnDefault{Func: string(d.Func)}
	}
	out := &protocol.ColumnDefault{}
	switch c.Type {
	case catalog.TypeText:
		out.Value = d.Text
	case catalog.TypeInt64:
		out.Value = d.Int64
	case catalog.TypeFloat64:
		out.Value = d.Float64
	case catalog.TypeBool:
		out.Value = d.Bool
	}
	return out
}

// tableInfo renders one table for introspection, resolving foreign key
// table IDs back to names.
func (a *dbAPI) tableInfo(ctx context.Context, t catalog.Table) (protocol.TableInfo, error) {
	info := protocol.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
	for _, c := range t.Columns {
		info.Columns = append(info.Columns, protocol.ColumnInfo{
			Name: c.Name, Type: string(c.Type), Nullable: c.Nullable,
			Format: c.Format, Default: defaultInfo(c),
		})
	}
	for _, idx := range t.Indexes {
		info.Indexes = append(info.Indexes, protocol.IndexDef{
			Name: idx.Name, Columns: idx.Columns, Unique: idx.Unique,
		})
	}
	for _, fk := range t.ForeignKeys {
		refName := t.Name // self-reference
		if fk.RefTableID != t.ID {
			ref, ok, err := a.cat.GetTableByID(ctx, fk.RefTableID)
			if err != nil {
				return protocol.TableInfo{}, err
			}
			if ok {
				refName = ref.Name
			} else {
				refName = fk.RefTableID // orphaned reference; expose the ID rather than lie
			}
		}
		info.ForeignKeys = append(info.ForeignKeys, protocol.ForeignKeyDef{
			Name: fk.Name, Columns: fk.Columns, RefTable: refName, RefColumns: fk.RefColumns,
		})
	}
	return info, nil
}

// tableOAS is tableInfo rendered for a TableOK response.
func (a *dbAPI) tableOAS(ctx context.Context, t catalog.Table) (*oas.TableInfo, error) {
	info, err := a.tableInfo(ctx, t)
	if err != nil {
		return nil, err
	}
	o := api.TableToOAS(info)
	return &o, nil
}

// -
// handlers
// -

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

func (a *dbAPI) ColumnCreate(ctx context.Context, req oas.OptColumnInfo, params oas.ColumnCreateParams) (oas.ColumnCreateRes, error) {
	if a.mode == catalog.ModeSchema {
		p := schemaManagedProblem()
		return (*oas.ColumnCreateUnprocessableEntity)(&p), nil
	}
	def, err := catColumnDef(protocol.ColumnDef(api.ColumnFromOAS(req.Or(oas.ColumnInfo{}))))
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
