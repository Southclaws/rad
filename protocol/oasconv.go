package protocol

// Bridge between the ergonomic hand-written wire types in this package and
// the ogen-generated types in ./oas. ogen owns the transport (HTTP client,
// server, JSON codecs, the OpenAPI contract); these plain structs stay the
// vocabulary the client runtime exposes and the code generator targets, so
// neither the generated clients nor the demo need to know ogen exists.
//
// The conversions are near mechanical: Rad's request and response shapes were
// designed alongside the contract, so the only real work is unwrapping ogen's
// Opt* fields and moving column values between Go's `any` (numbers decoded as
// json.Number to keep int64 precision) and ogen's raw JSON (jx.Raw).

import (
	"bytes"
	"encoding/json"

	"github.com/go-faster/jx"

	"rad/protocol/oas"
)

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
func RecordToMap(r oas.Record) Record {
	if r == nil {
		return nil
	}
	m := make(Record, len(r))
	for k, v := range r {
		m[k] = rawToAny(v)
	}
	return m
}

// MapToRecord converts a plain map into a result record.
func MapToRecord(m Record) oas.Record {
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
func RecordsToOAS(recs []Record) []oas.Record {
	out := make([]oas.Record, len(recs))
	for i, r := range recs {
		out[i] = MapToRecord(r)
	}
	return out
}

// RecordsFromOAS converts a query response's records back to the wire type.
func RecordsFromOAS(recs []oas.Record) []Record {
	out := make([]Record, len(recs))
	for i, r := range recs {
		out[i] = RecordToMap(r)
	}
	return out
}

// ── expressions ─────────────────────────────────────────────────────────────

func exprToOAS(e *Expr) *oas.Expr {
	if e == nil {
		return nil
	}
	o := &oas.Expr{Op: oas.ExprOp(e.Op)}
	for i := range e.Exprs {
		o.Exprs = append(o.Exprs, *exprToOAS(&e.Exprs[i]))
	}
	o.Expr = exprToOAS(e.Expr)
	if e.Column != "" {
		o.Column = oas.NewOptString(e.Column)
	}
	if e.Value != nil {
		o.Value = oas.Value(anyToRaw(e.Value))
	}
	return o
}

func exprToOpt(e *Expr) oas.OptExpr {
	if e == nil {
		return oas.OptExpr{}
	}
	return oas.NewOptExpr(*exprToOAS(e))
}

func exprFromOAS(o *oas.Expr) *Expr {
	if o == nil {
		return nil
	}
	e := &Expr{Op: string(o.Op), Column: o.Column.Or("")}
	for i := range o.Exprs {
		e.Exprs = append(e.Exprs, *exprFromOAS(&o.Exprs[i]))
	}
	e.Expr = exprFromOAS(o.Expr)
	if len(o.Value) != 0 {
		e.Value = rawToAny(jx.Raw(o.Value))
	}
	return e
}

func exprFromOpt(o oas.OptExpr) *Expr {
	if !o.Set {
		return nil
	}
	return exprFromOAS(&o.Value)
}

// ── order, aggregates, includes ─────────────────────────────────────────────

func ordersToOAS(in []Order) []oas.Order {
	if len(in) == 0 {
		return nil
	}
	out := make([]oas.Order, len(in))
	for i, o := range in {
		out[i] = oas.Order{Column: o.Column}
		if o.Desc {
			out[i].Desc = oas.NewOptBool(true)
		}
	}
	return out
}

func ordersFromOAS(in []oas.Order) []Order {
	if len(in) == 0 {
		return nil
	}
	out := make([]Order, len(in))
	for i, o := range in {
		out[i] = Order{Column: o.Column, Desc: o.Desc.Or(false)}
	}
	return out
}

func aggsToOAS(in []Agg) []oas.Aggregate {
	if len(in) == 0 {
		return nil
	}
	out := make([]oas.Aggregate, len(in))
	for i, a := range in {
		out[i] = oas.Aggregate{Fn: oas.AggregateFn(a.Fn), As: a.As}
		if a.Column != "" {
			out[i].Column = oas.NewOptString(a.Column)
		}
	}
	return out
}

func aggsFromOAS(in []oas.Aggregate) []Agg {
	if len(in) == 0 {
		return nil
	}
	out := make([]Agg, len(in))
	for i, a := range in {
		out[i] = Agg{Fn: string(a.Fn), Column: a.Column.Or(""), As: a.As}
	}
	return out
}

func includesToOAS(in []Include) []oas.Include {
	if len(in) == 0 {
		return nil
	}
	out := make([]oas.Include, len(in))
	for i, inc := range in {
		o := oas.Include{
			Fk:      inc.FK,
			Dir:     oas.IncludeDir(inc.Dir),
			As:      inc.As,
			Filter:  exprToOpt(inc.Filter),
			OrderBy: ordersToOAS(inc.OrderBy),
			Include: includesToOAS(inc.Include),
			Aggs:    aggsToOAS(inc.Aggs),
		}
		if inc.Limit != 0 {
			o.Limit = oas.NewOptInt(inc.Limit)
		}
		out[i] = o
	}
	return out
}

func includesFromOAS(in []oas.Include) []Include {
	if len(in) == 0 {
		return nil
	}
	out := make([]Include, len(in))
	for i, o := range in {
		out[i] = Include{
			FK:      o.Fk,
			Dir:     string(o.Dir),
			As:      o.As,
			Filter:  exprFromOpt(o.Filter),
			OrderBy: ordersFromOAS(o.OrderBy),
			Limit:   o.Limit.Or(0),
			Include: includesFromOAS(o.Include),
			Aggs:    aggsFromOAS(o.Aggs),
		}
	}
	return out
}

// ── reads ─────────────────────────────────────────────────────────────────

// ReadToOAS converts a shaped read into the generated Query type.
func ReadToOAS(r Read) oas.Query {
	q := oas.Query{
		Table:   r.Table,
		Filter:  exprToOpt(r.Filter),
		OrderBy: ordersToOAS(r.OrderBy),
		Include: includesToOAS(r.Include),
		Aggs:    aggsToOAS(r.Aggs),
	}
	if r.Offset != 0 {
		q.Offset = oas.NewOptInt(r.Offset)
	}
	if r.Limit != 0 {
		q.Limit = oas.NewOptInt(r.Limit)
	}
	return q
}

// ReadFromOAS converts a generated Query back into a shaped read.
func ReadFromOAS(q oas.Query) Read {
	return Read{
		Table:   q.Table,
		Filter:  exprFromOpt(q.Filter),
		OrderBy: ordersFromOAS(q.OrderBy),
		Offset:  q.Offset.Or(0),
		Limit:   q.Limit.Or(0),
		Include: includesFromOAS(q.Include),
		Aggs:    aggsFromOAS(q.Aggs),
	}
}

// ── introspection ───────────────────────────────────────────────────────────

// TablesToOAS converts table definitions for a TableList response.
func TablesToOAS(in []TableInfo) []oas.TableInfo {
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
func TablesFromOAS(in []oas.TableInfo) []TableInfo {
	out := make([]TableInfo, len(in))
	for i, t := range in {
		info := TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			info.Columns = append(info.Columns, ColumnInfo{
				Name: c.Name, Type: c.Type, Nullable: c.Nullable.Or(false), Format: c.Format.Or(""),
			})
		}
		out[i] = info
	}
	return out
}

// ── problems ────────────────────────────────────────────────────────────────

// ProblemToOAS converts a Problem into the generated type.
func ProblemToOAS(p Problem) oas.Problem {
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
func ProblemFromOAS(o oas.Problem) Problem {
	return Problem{
		Type:   o.Type,
		Title:  o.Title,
		Status: o.Status,
		Detail: o.Detail.Or(""),
		Code:   string(o.Code),
	}
}
