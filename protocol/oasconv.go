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

// ── the query graph ─────────────────────────────────────────────────────────

// QueryToOAS converts a query graph into the generated wire type.
func QueryToOAS(q Query) oas.Query {
	nodes := make(oas.QueryNodes, len(q.Nodes))
	for name, n := range q.Nodes {
		nodes[name] = nodeToOAS(n)
	}
	return oas.Query{
		Nodes: nodes,
		Root:  oas.Root{Node: q.Root.Node, Cardinality: oas.RootCardinality(q.Root.Cardinality)},
	}
}

// QueryFromOAS converts the generated wire type back into a query graph.
func QueryFromOAS(q oas.Query) Query {
	nodes := make(map[string]Node, len(q.Nodes))
	for name, n := range q.Nodes {
		nodes[name] = nodeFromOAS(n)
	}
	return Query{
		Nodes: nodes,
		Root:  Root{Node: q.Root.Node, Cardinality: string(q.Root.Cardinality)},
	}
}

func nodeToOAS(n Node) oas.Node {
	o := oas.Node{
		Kind:      oas.NodeKind(n.Kind),
		Table:     optString(n.Table),
		Scope:     optString(n.Scope),
		Input:     optString(n.Input),
		Left:      optString(n.Left),
		Right:     optString(n.Right),
		On:        exprToOpt(n.On),
		Predicate: exprToOpt(n.Predicate),
		Spread:    n.Spread,
	}
	if n.Join != "" {
		o.Join = oas.NewOptNodeJoin(oas.NodeJoin(n.Join))
	}
	for _, f := range n.Fields {
		o.Fields = append(o.Fields, oas.Field{As: f.As, Expr: *exprToOAS(&f.Expr)})
	}
	for _, g := range n.Groups {
		o.Groups = append(o.Groups, oas.GroupTerm{As: optString(g.As), Expr: *exprToOAS(&g.Expr)})
	}
	for _, a := range n.Aggs {
		o.Aggs = append(o.Aggs, oas.AggTerm{Fn: oas.AggTermFn(a.Fn), Arg: exprToOAS(a.Arg), As: a.As})
	}
	for _, t := range n.Terms {
		ot := oas.OrderTerm{Expr: *exprToOAS(&t.Expr)}
		if t.Desc {
			ot.Desc = oas.NewOptBool(true)
		}
		o.Terms = append(o.Terms, ot)
	}
	if n.Offset != nil {
		o.Offset = oas.NewOptInt(*n.Offset)
	}
	if n.Limit != nil {
		o.Limit = oas.NewOptInt(*n.Limit)
	}
	return o
}

func nodeFromOAS(o oas.Node) Node {
	n := Node{
		Kind:      string(o.Kind),
		Table:     o.Table.Or(""),
		Scope:     o.Scope.Or(""),
		Input:     o.Input.Or(""),
		Left:      o.Left.Or(""),
		Right:     o.Right.Or(""),
		On:        exprFromOpt(o.On),
		Predicate: exprFromOpt(o.Predicate),
		Spread:    o.Spread,
	}
	if j, ok := o.Join.Get(); ok {
		n.Join = string(j)
	}
	for _, f := range o.Fields {
		fe := f.Expr
		n.Fields = append(n.Fields, Field{As: f.As, Expr: *exprFromOAS(&fe)})
	}
	for _, g := range o.Groups {
		ge := g.Expr
		n.Groups = append(n.Groups, GroupTerm{As: g.As.Or(""), Expr: *exprFromOAS(&ge)})
	}
	for _, a := range o.Aggs {
		n.Aggs = append(n.Aggs, AggTerm{Fn: string(a.Fn), Arg: exprFromOAS(a.Arg), As: a.As})
	}
	for _, t := range o.Terms {
		te := t.Expr
		n.Terms = append(n.Terms, OrderTerm{Expr: *exprFromOAS(&te), Desc: t.Desc.Or(false)})
	}
	if v, ok := o.Offset.Get(); ok {
		n.Offset = &v
	}
	if v, ok := o.Limit.Get(); ok {
		n.Limit = &v
	}
	return n
}

func exprToOAS(e *Expr) *oas.Expr {
	if e == nil {
		return nil
	}
	o := &oas.Expr{
		Kind:   oas.ExprKind(e.Kind),
		Scope:  optString(e.Scope),
		Column: optString(e.Column),
		Op:     optString(e.Op),
		Fn:     optString(e.Fn),
		To:     optString(e.To),
		Node:   optString(e.Node),
		Expr:   exprToOAS(e.Expr),
		Left:   exprToOAS(e.Left),
		Right:  exprToOAS(e.Right),
	}
	for i := range e.Args {
		o.Args = append(o.Args, *exprToOAS(&e.Args[i]))
	}
	// Always populate Value: the generated codec emits the field
	// unconditionally, and an empty raw value would render as the malformed
	// body `"value":}`. Non-literal kinds carry an explicit JSON null, which
	// decodes back to a nil value.
	o.Value = oas.Value(anyToRaw(e.Value))
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
	e := &Expr{
		Kind:   string(o.Kind),
		Scope:  o.Scope.Or(""),
		Column: o.Column.Or(""),
		Op:     o.Op.Or(""),
		Fn:     o.Fn.Or(""),
		To:     o.To.Or(""),
		Node:   o.Node.Or(""),
		Expr:   exprFromOAS(o.Expr),
		Left:   exprFromOAS(o.Left),
		Right:  exprFromOAS(o.Right),
	}
	for i := range o.Args {
		e.Args = append(e.Args, *exprFromOAS(&o.Args[i]))
	}
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

func optString(s string) oas.OptString {
	if s == "" {
		return oas.OptString{}
	}
	return oas.NewOptString(s)
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
