// Package protocol defines Rad's transport-neutral wire vocabulary: the
// rad:// connection URI, LIR payloads, and JSON values shared by clients and
// servers. It contains no HTTP routing, OpenAPI, or generated HTTP types;
// those live in package api. It has no engine imports, and LIR JSON validation
// is its only contract dependency outside the standard library.
//
// # Connection URIs
//
//	rad://localhost            → http://localhost:7237
//	rad://db.internal:9000     → http://db.internal:9000
//	rads://db.example.com      → https://db.example.com:7237
//
// Rad speaks plain HTTP, so rad:// resolves to http. Front it with your own
// proxy for encryption or authentication in production and use rads:// — the
// client then dials that proxy over HTTPS. Rad itself never terminates TLS.
//
// Errors are RFC 7807 Problem Details (application/problem+json). The
// "code" extension member distinguishes retryable conflicts from
// constraint violations; "type" is a stable URI per code.
//
// # Values
//
// Column values travel as plain JSON: string, number, boolean, or null.
// Clients decode numbers with json.Number to keep int64 precision; the
// server coerces incoming numbers using the catalog's column types.
package protocol

import (
	"fmt"
	"net/url"
)

// DefaultPort is Rad's default port. Override the listen address with
// RAD_ADDR on the server; clients name a port in the URI.
const DefaultPort = 7237

// ParseURL parses a rad(s):// connection URI into an http(s) base URL. rad://
// resolves to http; rads:// resolves to https, for when a TLS-terminating
// proxy sits in front of the server. Rad itself never terminates TLS.
func ParseURL(raw string) (string, error) {
	u, err := url.Parse(raw)
	if err != nil {
		return "", fmt.Errorf("protocol: invalid connection URI %q: %w", raw, err)
	}
	var scheme string
	switch u.Scheme {
	case "rad":
		scheme = "http"
	case "rads":
		scheme = "https"
	default:
		return "", fmt.Errorf("protocol: connection URI must use rad:// or rads://, got %q", raw)
	}
	if u.Hostname() == "" {
		return "", fmt.Errorf("protocol: connection URI %q has no host", raw)
	}
	if u.User != nil || u.RawQuery != "" || (u.Path != "" && u.Path != "/") {
		return "", fmt.Errorf("protocol: connection URI must be rad(s)://host[:port], got %q", raw)
	}
	host := u.Host // preserves [::1] brackets and any explicit port
	if u.Port() == "" {
		host = fmt.Sprintf("%s:%d", u.Host, DefaultPort)
	}
	return scheme + "://" + host, nil
}

// Error codes carried in the Problem "code" extension member.
const (
	CodeInvalid         = "invalid"          // malformed request, unknown table/column, constraint violation
	CodeExecutionFailed = "execution_failed" // a valid query failed on the data it met (division by zero, cardinality assertion)
	CodeNotFound        = "not_found"
	CodeConflict        = "conflict" // optimistic transaction conflict — retry
	CodeInternal        = "internal"
)

// ProblemTypeBase prefixes the "type" URI of every problem; the code is
// appended (e.g. urn:rad:problem:conflict). A URN, deliberately: RFC 7807
// type URIs need not be dereferenceable, and Rad has no website.
const ProblemTypeBase = "urn:rad:problem:"

// Problem is an RFC 7807 Problem Details body — every non-2xx response.
// Code is an extension member mirroring the last segment of Type so clients
// can switch on it without URI parsing.
type Problem struct {
	Type   string `json:"type"`
	Title  string `json:"title"`
	Status int    `json:"status"`
	Detail string `json:"detail,omitempty"`
	Code   string `json:"code"`
}

// NewProblem builds a Problem for a code, status, and detail message.
func NewProblem(code string, status int, detail string) Problem {
	titles := map[string]string{
		CodeInvalid:         "Invalid Request",
		CodeExecutionFailed: "Query Execution Failed",
		CodeNotFound:        "Not Found",
		CodeConflict:        "Transaction Conflict",
		CodeInternal:        "Internal Server Error",
	}
	title := titles[code]
	if title == "" {
		title = code
	}
	return Problem{
		Type:   ProblemTypeBase + code,
		Title:  title,
		Status: status,
		Detail: detail,
		Code:   code,
	}
}

// Query is LIR's flat encoding of a single-consumer relation tree: named
// inline relation definitions plus a root selector. Node names carry no
// sharing or materialisation identity. The schema is lir.schema.json; the
// semantics are documented in home/content/docs/lir.mdx. The contract is
// intentionally unversioned while LIR remains an internal, freely breakable
// API surface.
type Query struct {
	Nodes map[string]Node `json:"nodes"`
	// Bindings are named relational values: each identifies the root of its
	// defining tree in Nodes. A binding denotes one statement-local
	// committed bag; every ref node observes the same bag under a fresh
	// scope. Binding names, node ids, and scope labels are separate
	// namespaces.
	Bindings map[string]Binding `json:"bindings,omitempty"`
	Root     Root               `json:"root"`
}

// Binding names one relational value by its defining root node.
type Binding struct {
	Node string `json:"node"`
}

// Root selects the result node and how it materialises: many | first |
// exactly_one | scalar.
type Root struct {
	Node        string `json:"node"`
	Cardinality string `json:"cardinality"`
}

// Node is the convenient handwritten form of one closed relation-operator
// variant. The Schemancer-generated transport represents these as a strict
// tagged union.
type Node struct {
	Kind string `json:"kind"`

	Table     string      `json:"table,omitempty"`     // scan
	Scope     string      `json:"scope,omitempty"`     // scan/ref (required), project, aggregate
	Binding   string      `json:"binding,omitempty"`   // ref
	Input     string      `json:"input,omitempty"`     // filter, project, aggregate, order, slice
	Left      string      `json:"left,omitempty"`      // join
	Right     string      `json:"right,omitempty"`     // join
	Join      string      `json:"join,omitempty"`      // join: inner | left
	On        *Expr       `json:"on,omitempty"`        // join
	Predicate *Expr       `json:"predicate,omitempty"` // filter
	Spread    []string    `json:"spread,omitempty"`    // project
	Fields    []Field     `json:"fields,omitempty"`    // project
	Groups    []GroupTerm `json:"groups,omitempty"`    // aggregate
	Aggs      []AggTerm   `json:"aggs,omitempty"`      // aggregate
	Terms     []OrderTerm `json:"terms,omitempty"`     // order
	Offset    *int        `json:"offset,omitempty"`    // slice
	Limit     *int        `json:"limit,omitempty"`     // slice; absent = unlimited, 0 = no rows
}

// Expr computes exactly one typed datum. The generated transport is a closed
// tagged union; this handwritten form keeps graph builders compact. Crossings may
// produce row or array datums as well as scalars.
type Expr struct {
	Kind string `json:"kind"`

	Value  any    `json:"value,omitempty"`  // lit
	Scope  string `json:"scope,omitempty"`  // col
	Column string `json:"column,omitempty"` // col
	Op     string `json:"op,omitempty"`     // unary, binary
	Expr   *Expr  `json:"expr,omitempty"`   // unary, cast
	Left   *Expr  `json:"left,omitempty"`   // binary
	Right  *Expr  `json:"right,omitempty"`  // binary
	To     string `json:"to,omitempty"`     // cast
	Node   string `json:"node,omitempty"`   // crossings
}

// Field is one projected attribute.
type Field struct {
	As   string `json:"as"`
	Expr Expr   `json:"expr"`
}

// GroupTerm is one grouping attribute; As defaults to a bare column
// reference's name.
type GroupTerm struct {
	As   string `json:"as,omitempty"`
	Expr Expr   `json:"expr"`
}

// AggTerm is one fold, named As in the output. Arg is nil for count over
// rows.
type AggTerm struct {
	Fn  string `json:"fn"`
	Arg *Expr  `json:"arg,omitempty"`
	As  string `json:"as"`
}

// OrderTerm is one ordering term.
type OrderTerm struct {
	Expr Expr `json:"expr"`
	Desc bool `json:"desc,omitempty"`
}

// Record is one result row: scalar attributes plus nested relations
// (objects for first fields — null when absent — and arrays for array
// fields).
type Record = map[string]any

type MigrateRequest struct {
	// Schema is the schema.rad source (YAML).
	Schema string `json:"schema"`
}

type MigrateResponse struct {
	Steps []string `json:"steps"`
}

type QueryResponse struct {
	Records []Record `json:"records"`
}

type CreateRequest struct {
	Table  string         `json:"table"`
	Values map[string]any `json:"values"`
}

type UpdateRequest struct {
	Table string         `json:"table"`
	Key   map[string]any `json:"key"`
	Set   map[string]any `json:"set"`
	// Clear lists nullable columns to set to NULL.
	Clear []string `json:"clear,omitempty"`
}

type DeleteRequest struct {
	Table string         `json:"table"`
	Key   map[string]any `json:"key"`
}

// RecordResponse carries a single record; Found is false (and Record nil)
// when the target row does not exist.
type RecordResponse struct {
	Found  bool   `json:"found"`
	Record Record `json:"record,omitempty"`
}

type DeleteResponse struct {
	Found bool `json:"found"`
}

type TxResponse struct {
	ID string `json:"id"`
}

// TableInfo describes one table for introspection (GET /tables and the
// catalog mutation responses). It is the definition vocabulary read back:
// indexes and foreign keys reuse the def shapes, with foreign keys naming
// their referenced table rather than carrying internal IDs.
type TableInfo struct {
	Name        string          `json:"name"`
	Columns     []ColumnInfo    `json:"columns"`
	PrimaryKey  []string        `json:"primary_key"`
	Indexes     []IndexDef      `json:"indexes,omitempty"`
	ForeignKeys []ForeignKeyDef `json:"foreign_keys,omitempty"`
}

type ColumnInfo struct {
	Name     string         `json:"name"`
	Type     string         `json:"type"`
	Nullable bool           `json:"nullable,omitempty"`
	Format   string         `json:"format,omitempty"`
	Default  *ColumnDefault `json:"default,omitempty"`
}

// Catalog mutation vocabulary: the definition shapes accepted by the
// imperative catalog endpoints (direct-mode databases only). These mirror
// the engine's definitions with plain JSON values; the server owns all
// coercion and validation.

// TableDef defines a new table for TableCreate.
type TableDef struct {
	Name        string          `json:"name"`
	Columns     []ColumnDef     `json:"columns"`
	PrimaryKey  []string        `json:"primary_key"`
	Indexes     []IndexDef      `json:"indexes,omitempty"`
	ForeignKeys []ForeignKeyDef `json:"foreign_keys,omitempty"`
}

// ColumnDef defines one column. Type is one of text, int64, float64, bool.
type ColumnDef struct {
	Name     string         `json:"name"`
	Type     string         `json:"type"`
	Nullable bool           `json:"nullable,omitempty"`
	Format   string         `json:"format,omitempty"`
	Default  *ColumnDefault `json:"default,omitempty"`
}

// ColumnDefault is either a builtin generator (func: uuid | now_ms) or a
// literal of the column's type (value); exactly one is set.
type ColumnDefault struct {
	Func  string `json:"func,omitempty"`
	Value any    `json:"value,omitempty"`
}

// IndexDef defines one secondary index.
type IndexDef struct {
	Name    string   `json:"name"`
	Columns []string `json:"columns"`
	Unique  bool     `json:"unique,omitempty"`
}

// ForeignKeyDef defines one foreign key. RefColumns must be the referenced
// table's full primary key.
type ForeignKeyDef struct {
	Name       string   `json:"name"`
	Columns    []string `json:"columns"`
	RefTable   string   `json:"ref_table"`
	RefColumns []string `json:"ref_columns"`
}
