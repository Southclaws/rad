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

// RecordsToOAS converts a list of result records for a query response.
func RecordsToOAS(recs []protocol.Record) []oas.Record {
	out := make([]oas.Record, len(recs))
	for i, r := range recs {
		out[i] = MapToRecord(r)
	}
	return out
}

// RecordsFromOAS converts a query response's records back to the wire type.
func RecordsFromOAS(recs []oas.Record) []protocol.Record {
	out := make([]protocol.Record, len(recs))
	for i, r := range recs {
		out[i] = RecordToMap(r)
	}
	return out
}

// ── introspection ───────────────────────────────────────────────────────────

// TablesToOAS converts table definitions for a TableList response.
func TablesToOAS(in []protocol.TableInfo) []oas.TableInfo {
	out := make([]oas.TableInfo, len(in))
	for i, t := range in {
		o := oas.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			col := oas.ColumnInfo{Name: c.Name, Type: c.Type}
			if c.Nullable {
				col.Nullable = oas.NewOptBool(true)
			}
			if c.Format != "" {
				col.Format = oas.NewOptString(c.Format)
			}
			o.Columns = append(o.Columns, col)
		}
		out[i] = o
	}
	return out
}

// TablesFromOAS converts a TableList response back to wire types.
func TablesFromOAS(in []oas.TableInfo) []protocol.TableInfo {
	out := make([]protocol.TableInfo, len(in))
	for i, t := range in {
		info := protocol.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			info.Columns = append(info.Columns, protocol.ColumnInfo{
				Name: c.Name, Type: c.Type, Nullable: c.Nullable.Or(false), Format: c.Format.Or(""),
			})
		}
		out[i] = info
	}
	return out
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
