// Package protocol defines Rad's wire protocol: the rad:// connection URI
// and the JSON request/response types exchanged between clients and a Rad
// server over HTTP. It is shared vocabulary for the client runtime and the
// server — pure data, stdlib only, no engine imports.
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
// # Endpoints
//
// Unversioned by design — this is a proof of concept with no compatibility
// guarantees.
//
//	GET  /health                        liveness
//	GET  /tables                        table definitions
//	POST /migrate                       MigrateRequest  → MigrateResponse
//	POST /query                         Read            → QueryResponse
//	POST /create                        CreateRequest   → RecordResponse
//	POST /update                        UpdateRequest   → RecordResponse
//	POST /delete                        DeleteRequest   → DeleteResponse
//	POST /tx                            —               → TxResponse
//	POST /tx/{id}/query|create|update|delete            as above
//	POST /tx/{id}/commit                —               → empty
//	POST /tx/{id}/rollback              —               → empty
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
	CodeInvalid  = "invalid" // malformed request, unknown table/column, constraint violation
	CodeNotFound = "not_found"
	CodeConflict = "conflict" // optimistic transaction conflict — retry
	CodeInternal = "internal"
)

// ProblemContentType is the media type of error responses (RFC 7807).
const ProblemContentType = "application/problem+json"

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
		CodeInvalid:  "Invalid Request",
		CodeNotFound: "Not Found",
		CodeConflict: "Transaction Conflict",
		CodeInternal: "Internal Server Error",
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

// Expr is a filter expression — a tagged union selected by Op.
//
//	{"op":"eq","column":"status","value":"todo"}
//	{"op":"and","exprs":[...]}
//	{"op":"is_null","column":"assignee_id"}
type Expr struct {
	Op string `json:"op"` // and, or, not, eq, ne, lt, lte, gt, gte, is_null

	Exprs  []Expr `json:"exprs,omitempty"`  // and, or
	Expr   *Expr  `json:"expr,omitempty"`   // not
	Column string `json:"column,omitempty"` // comparisons, is_null
	Value  any    `json:"value,omitempty"`  // comparisons
}

// Order is one ORDER BY term.
type Order struct {
	Column string `json:"column"`
	Desc   bool   `json:"desc,omitempty"`
}

// Agg is one aggregate term: a fold over a column, named in the result. Fn
// is count | sum | avg | min | max; Column is omitted for count() over rows.
type Agg struct {
	Fn     string `json:"fn"`
	Column string `json:"column,omitempty"`
	As     string `json:"as"`
}

// Include embeds a related relation in each result record. When Aggs is set,
// the relation is folded into one object of scalars under As instead of a
// record array (children only) — the same shape switch Read.Aggs makes at the
// root.
type Include struct {
	FK  string `json:"fk"`  // foreign key name
	Dir string `json:"dir"` // "parent" or "children"
	As  string `json:"as"`  // output field name

	Filter  *Expr     `json:"filter,omitempty"` // children only
	OrderBy []Order   `json:"order_by,omitempty"`
	Limit   int       `json:"limit,omitempty"`
	Include []Include `json:"include,omitempty"`
	Aggs    []Agg     `json:"aggs,omitempty"`
}

// Read is a shaped read: the query form of Rad's QIR on the wire. It is the
// single query operation — asking for Aggs folds the matching rows into one
// scalar record rather than returning rows, so aggregation never needs its
// own endpoint or verb.
type Read struct {
	Table   string    `json:"table"`
	Filter  *Expr     `json:"filter,omitempty"`
	OrderBy []Order   `json:"order_by,omitempty"`
	Offset  int       `json:"offset,omitempty"`
	Limit   int       `json:"limit,omitempty"`
	Include []Include `json:"include,omitempty"`
	Aggs    []Agg     `json:"aggs,omitempty"`
}

// Record is one result row: column values plus nested includes (objects for
// parent includes — null when the FK is NULL — and arrays for children).
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

// TableInfo describes one table for GET /tables.
type TableInfo struct {
	Name       string       `json:"name"`
	Columns    []ColumnInfo `json:"columns"`
	PrimaryKey []string     `json:"primary_key"`
}

type ColumnInfo struct {
	Name     string `json:"name"`
	Type     string `json:"type"`
	Nullable bool   `json:"nullable,omitempty"`
	Format   string `json:"format,omitempty"`
}
