// Package protocol defines RAD's wire protocol: the rad:// connection URI
// and the JSON request/response types exchanged between clients and a RAD
// server over HTTP. It is shared vocabulary for the client runtime and the
// server — pure data, stdlib only, no engine imports.
//
// # Connection URIs
//
//	rad://localhost            → http://localhost:7237
//	rad://db.internal:9000     → http://db.internal:9000
//	rads://db.example.com      → https://db.example.com:7237
//
// rads is rad over TLS. RAD itself never terminates TLS — a reverse proxy
// in front of the server does.
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
//	POST /get                           GetRequest      → RecordResponse
//	POST /create                        CreateRequest   → RecordResponse
//	POST /update                        UpdateRequest   → RecordResponse
//	POST /delete                        DeleteRequest   → DeleteResponse
//	POST /tx                            —               → TxResponse
//	POST /tx/{id}/query|get|create|update|delete        as above
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

// DefaultPort is RAD's default port. Set in stone.
const DefaultPort = 7237

// ParseURL parses a rad(s):// connection URI into an http(s) base URL.
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
// appended (e.g. https://rad.dev/problems/conflict).
const ProblemTypeBase = "https://rad.dev/problems/"

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

// Include embeds a related relation in each result record.
type Include struct {
	FK  string `json:"fk"`  // foreign key name
	Dir string `json:"dir"` // "parent" or "children"
	As  string `json:"as"`  // output field name

	Filter  *Expr     `json:"filter,omitempty"` // children only
	OrderBy []Order   `json:"order_by,omitempty"`
	Limit   int       `json:"limit,omitempty"`
	Include []Include `json:"include,omitempty"`
}

// Read is a shaped read: the query form of RAD's QIR on the wire.
type Read struct {
	Table   string    `json:"table"`
	Filter  *Expr     `json:"filter,omitempty"`
	OrderBy []Order   `json:"order_by,omitempty"`
	Offset  int       `json:"offset,omitempty"`
	Limit   int       `json:"limit,omitempty"`
	Include []Include `json:"include,omitempty"`
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

type GetRequest struct {
	Table string         `json:"table"`
	Key   map[string]any `json:"key"` // exactly the primary key columns
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
