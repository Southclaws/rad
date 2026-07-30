package api

import (
	"github.com/go-faster/jx"

	"github.com/Southclaws/rad/clients/go/api/oas"
	"github.com/Southclaws/rad/clients/go/protocol"
)

// DefaultToOAS converts a column default.
func DefaultToOAS(d *protocol.ColumnDefault) oas.OptColumnDefault {
	if d == nil {
		return oas.OptColumnDefault{}
	}
	// The generated encoder emits `value` unconditionally and an empty raw
	// is invalid JSON, so an absent value must be an explicit null.
	o := oas.ColumnDefault{Value: oas.Value(anyToRaw(d.Value))}
	if d.Func != "" {
		o.Func = oas.NewOptString(d.Func)
	}
	return oas.NewOptColumnDefault(o)
}

// DefaultFromOAS converts a column default back to wire types.
func DefaultFromOAS(o oas.OptColumnDefault) *protocol.ColumnDefault {
	if !o.Set {
		return nil
	}
	return &protocol.ColumnDefault{
		Func:  o.Value.Func.Or(""),
		Value: rawToAny(jx.Raw(o.Value.Value)),
	}
}

// ColumnToOAS converts one column definition.
func ColumnToOAS(c protocol.ColumnInfo) oas.ColumnInfo {
	col := oas.ColumnInfo{ID: int64(c.ID), Name: c.Name, Type: c.Type, Default: DefaultToOAS(c.Default)}
	if c.Nullable {
		col.Nullable = oas.NewOptBool(true)
	}
	if c.Format != "" {
		col.Format = oas.NewOptString(c.Format)
	}
	return col
}

// ColumnFromOAS converts one column definition back to wire types.
func ColumnFromOAS(c oas.ColumnInfo) protocol.ColumnInfo {
	return protocol.ColumnInfo{
		ID: uint32(c.ID), Name: c.Name, Type: c.Type, Nullable: c.Nullable.Or(false),
		Format: c.Format.Or(""), Default: DefaultFromOAS(c.Default),
	}
}

// ColumnDefToOAS converts one direct catalog column definition.
func ColumnDefToOAS(c protocol.ColumnDef) oas.ColumnDef {
	column := oas.ColumnDef{Name: c.Name, Type: c.Type, Default: DefaultToOAS(c.Default)}
	if c.ID != 0 {
		column.ID = oas.NewOptInt64(int64(c.ID))
	}
	if c.Nullable {
		column.Nullable = oas.NewOptBool(true)
	}
	if c.Format != "" {
		column.Format = oas.NewOptString(c.Format)
	}
	return column
}

// ColumnDefFromOAS converts one direct catalog column definition back to wire types.
func ColumnDefFromOAS(c oas.ColumnDef) protocol.ColumnDef {
	return protocol.ColumnDef{
		ID: uint32(c.ID.Or(0)), Name: c.Name, Type: c.Type, Nullable: c.Nullable.Or(false),
		Format: c.Format.Or(""), Default: DefaultFromOAS(c.Default),
	}
}

// IndexToOAS converts one index definition.
func IndexToOAS(i protocol.IndexDef) oas.IndexInfo {
	o := oas.IndexInfo{Name: i.Name, Columns: i.Columns}
	if i.Unique {
		o.Unique = oas.NewOptBool(true)
	}
	return o
}

// IndexFromOAS converts one index definition back to wire types.
func IndexFromOAS(o oas.IndexInfo) protocol.IndexDef {
	return protocol.IndexDef{Name: o.Name, Columns: o.Columns, Unique: o.Unique.Or(false)}
}

func fkToOAS(fk protocol.ForeignKeyDef) oas.ForeignKeyInfo {
	return oas.ForeignKeyInfo{Name: fk.Name, Columns: fk.Columns, RefTable: fk.RefTable, RefColumns: fk.RefColumns}
}

func fkFromOAS(o oas.ForeignKeyInfo) protocol.ForeignKeyDef {
	return protocol.ForeignKeyDef{Name: o.Name, Columns: o.Columns, RefTable: o.RefTable, RefColumns: o.RefColumns}
}

// TableToOAS converts one table's introspection info.
func TableToOAS(t protocol.TableInfo) oas.TableInfo {
	o := oas.TableInfo{ID: int64(t.ID), Name: t.Name, PrimaryKey: t.PrimaryKey}
	for _, c := range t.Columns {
		o.Columns = append(o.Columns, ColumnToOAS(c))
	}
	for _, i := range t.Indexes {
		o.Indexes = append(o.Indexes, IndexToOAS(i))
	}
	for _, fk := range t.ForeignKeys {
		o.ForeignKeys = append(o.ForeignKeys, fkToOAS(fk))
	}
	return o
}

// TableFromOAS converts one table's introspection info back to wire types.
func TableFromOAS(t oas.TableInfo) protocol.TableInfo {
	info := protocol.TableInfo{ID: uint32(t.ID), Name: t.Name, PrimaryKey: t.PrimaryKey}
	for _, c := range t.Columns {
		info.Columns = append(info.Columns, ColumnFromOAS(c))
	}
	for _, i := range t.Indexes {
		info.Indexes = append(info.Indexes, IndexFromOAS(i))
	}
	for _, fk := range t.ForeignKeys {
		info.ForeignKeys = append(info.ForeignKeys, fkFromOAS(fk))
	}
	return info
}

// TablesToOAS converts table definitions for a TableList response.
func TablesToOAS(in []protocol.TableInfo) []oas.TableInfo {
	out := make([]oas.TableInfo, len(in))
	for i, table := range in {
		out[i] = TableToOAS(table)
	}
	return out
}

// TablesFromOAS converts a TableList response back to wire types.
func TablesFromOAS(in []oas.TableInfo) []protocol.TableInfo {
	out := make([]protocol.TableInfo, len(in))
	for i, table := range in {
		out[i] = TableFromOAS(table)
	}
	return out
}

// TableDefToOAS converts a table definition for a TableCreate request.
func TableDefToOAS(d protocol.TableDef) oas.TableDef {
	o := oas.TableDef{Name: d.Name, PrimaryKey: d.PrimaryKey}
	if d.ID != 0 {
		o.ID = oas.NewOptInt64(int64(d.ID))
	}
	for _, column := range d.Columns {
		o.Columns = append(o.Columns, ColumnDefToOAS(column))
	}
	for _, index := range d.Indexes {
		o.Indexes = append(o.Indexes, IndexToOAS(index))
	}
	for _, foreignKey := range d.ForeignKeys {
		o.ForeignKeys = append(o.ForeignKeys, fkToOAS(foreignKey))
	}
	return o
}

// TableDefFromOAS converts a TableCreate request body back to wire types.
func TableDefFromOAS(o oas.TableDef) protocol.TableDef {
	d := protocol.TableDef{ID: uint32(o.ID.Or(0)), Name: o.Name, PrimaryKey: o.PrimaryKey}
	for _, column := range o.Columns {
		d.Columns = append(d.Columns, ColumnDefFromOAS(column))
	}
	for _, index := range o.Indexes {
		d.Indexes = append(d.Indexes, IndexFromOAS(index))
	}
	for _, foreignKey := range o.ForeignKeys {
		d.ForeignKeys = append(d.ForeignKeys, fkFromOAS(foreignKey))
	}
	return d
}

func SchemaDocumentToOAS(document protocol.SchemaDocument) oas.SchemaDocument {
	tables := make([]oas.TableDef, len(document.Tables))
	for i, table := range document.Tables {
		tables[i] = TableDefToOAS(table)
	}
	return oas.SchemaDocument{Tables: tables}
}

func SchemaDocumentFromOAS(document oas.SchemaDocument) protocol.SchemaDocument {
	tables := make([]protocol.TableDef, len(document.Tables))
	for i, table := range document.Tables {
		tables[i] = TableDefFromOAS(table)
	}
	return protocol.SchemaDocument{Tables: tables}
}

func SchemaChangeFromOAS(change oas.SchemaChange) protocol.SchemaChange {
	return protocol.SchemaChange{
		Kind: change.Kind, Summary: change.Summary,
		Table: change.Table.Or(""), Column: change.Column.Or(""),
	}
}

func SchemaFindingFromOAS(finding oas.SchemaFinding) protocol.SchemaFinding {
	return protocol.SchemaFinding{
		Kind: finding.Kind, Summary: finding.Summary,
		Table: finding.Table.Or(""), Column: finding.Column.Or(""),
		Rows: uint64(finding.Rows.Or(0)),
	}
}

func SchemaChangesFromOAS(changes []oas.SchemaChange) []protocol.SchemaChange {
	out := make([]protocol.SchemaChange, len(changes))
	for i, change := range changes {
		out[i] = SchemaChangeFromOAS(change)
	}
	return out
}

func SchemaFindingsFromOAS(findings []oas.SchemaFinding) []protocol.SchemaFinding {
	out := make([]protocol.SchemaFinding, len(findings))
	for i, finding := range findings {
		out[i] = SchemaFindingFromOAS(finding)
	}
	return out
}

func SchemaStateFromOAS(state oas.SchemaState) protocol.SchemaState {
	return protocol.SchemaState{
		SchemaVersion: uint64(state.SchemaVersion), SchemaHash: state.SchemaHash,
		Schema: SchemaDocumentFromOAS(state.Schema),
	}
}
