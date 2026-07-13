// Package api owns Rad's HTTP API contract and the conversions between its
// ogen-generated representations and the transport-neutral wire protocol.
package api

// Bridge between the ergonomic hand-written HTTP types in this package and
// the ogen-generated types in ./oas. LIR query conversion lives separately in
// lirjson.go because its JSON Schema is intentionally not part of OpenAPI.
//
// The conversions are near mechanical: Rad's request and response shapes were
// designed alongside the contract, so the only real work is unwrapping ogen's
// Opt* fields and moving column values between Go's `any` (numbers decoded as
// json.Number to keep int64 precision) and ogen's raw JSON (jx.Raw).

import (
	"bytes"
	"encoding/json"

	"github.com/go-faster/jx"

	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/protocol"
)

// ProblemContentType is the HTTP media type for RFC 7807 responses.
const ProblemContentType = "application/problem+json"

// ── values ────────────────────────────────────────────────────────────────

// rawToAny decodes a raw JSON value into a Go value, preserving numbers as
// json.Number so int64 columns keep full precision. An empty raw is null.
func rawToAny(raw jx.Raw) any {
	if len(raw) == 0 {
		return nil
	}
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		return nil
	}
	return v
}

// anyToRaw encodes a Go value as a raw JSON value. Column values are scalars
// or, for embedded relations, nested objects and arrays, all of which marshal
// cleanly; an encode failure yields JSON null rather than a broken document.
func anyToRaw(v any) jx.Raw {
	raw, err := json.Marshal(v)
	if err != nil {
		return jx.Raw("null")
	}
	return jx.Raw(raw)
}

// ── cells and records ───────────────────────────────────────────────────────

// CellsToMap converts a request's cells (primary key, new values, update
// assignments) into a plain map with numbers preserved as json.Number.
func CellsToMap(c oas.Cells) map[string]any {
	if c == nil {
		return nil
	}
	m := make(map[string]any, len(c))
	for k, v := range c {
		m[k] = rawToAny(jx.Raw(v))
	}
	return m
}

// MapToCells converts a plain map into request cells.
func MapToCells(m map[string]any) oas.Cells {
	if m == nil {
		return nil
	}
	c := make(oas.Cells, len(m))
	for k, v := range m {
		c[k] = oas.Value(anyToRaw(v))
	}
	return c
}

// RecordToMap converts a result record into a plain map.
func RecordToMap(r oas.Record) protocol.Record {
	if r == nil {
		return nil
	}
	m := make(protocol.Record, len(r))
	for k, v := range r {
		m[k] = rawToAny(v)
	}
	return m
}

// MapToRecord converts a plain map into a result record.
func MapToRecord(m protocol.Record) oas.Record {
	if m == nil {
		return nil
	}
	r := make(oas.Record, len(m))
	for k, v := range m {
		r[k] = anyToRaw(v)
	}
	return r
}

// ── introspection and catalog definitions ──────────────────────────────────

// DefaultToOAS converts a column default.
func DefaultToOAS(d *protocol.ColumnDefault) oas.OptColumnDefault {
	if d == nil {
		return oas.OptColumnDefault{}
	}
	// The generated encoder emits `value` unconditionally and an empty raw
	// is invalid JSON, so an absent value must be an explicit null (which
	// anyToRaw produces for nil).
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
	col := oas.ColumnInfo{Name: c.Name, Type: c.Type, Default: DefaultToOAS(c.Default)}
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
		Name: c.Name, Type: c.Type, Nullable: c.Nullable.Or(false),
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
	o := oas.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
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
	info := protocol.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
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
	for i, t := range in {
		out[i] = TableToOAS(t)
	}
	return out
}

// TablesFromOAS converts a TableList response back to wire types.
func TablesFromOAS(in []oas.TableInfo) []protocol.TableInfo {
	out := make([]protocol.TableInfo, len(in))
	for i, t := range in {
		out[i] = TableFromOAS(t)
	}
	return out
}

// TableDefToOAS converts a table definition for a TableCreate request.
func TableDefToOAS(d protocol.TableDef) oas.TableDef {
	o := oas.TableDef{Name: d.Name, PrimaryKey: d.PrimaryKey}
	for _, c := range d.Columns {
		o.Columns = append(o.Columns, ColumnToOAS(protocol.ColumnInfo(c)))
	}
	for _, i := range d.Indexes {
		o.Indexes = append(o.Indexes, IndexToOAS(i))
	}
	for _, fk := range d.ForeignKeys {
		o.ForeignKeys = append(o.ForeignKeys, fkToOAS(fk))
	}
	return o
}

// TableDefFromOAS converts a TableCreate request body back to wire types.
func TableDefFromOAS(o oas.TableDef) protocol.TableDef {
	d := protocol.TableDef{Name: o.Name, PrimaryKey: o.PrimaryKey}
	for _, c := range o.Columns {
		d.Columns = append(d.Columns, protocol.ColumnDef(ColumnFromOAS(c)))
	}
	for _, i := range o.Indexes {
		d.Indexes = append(d.Indexes, IndexFromOAS(i))
	}
	for _, fk := range o.ForeignKeys {
		d.ForeignKeys = append(d.ForeignKeys, fkFromOAS(fk))
	}
	return d
}

// ── problems ────────────────────────────────────────────────────────────────

// ProblemToOAS converts a Problem into the generated type.
func ProblemToOAS(p protocol.Problem) oas.Problem {
	o := oas.Problem{
		Type:   p.Type,
		Title:  p.Title,
		Status: p.Status,
		Code:   oas.ProblemCode(p.Code),
	}
	if p.Detail != "" {
		o.Detail = oas.NewOptString(p.Detail)
	}
	return o
}

// ProblemFromOAS converts a generated Problem back into the wire type.
func ProblemFromOAS(o oas.Problem) protocol.Problem {
	return protocol.Problem{
		Type:   o.Type,
		Title:  o.Title,
		Status: o.Status,
		Detail: o.Detail.Or(""),
		Code:   string(o.Code),
	}
}
