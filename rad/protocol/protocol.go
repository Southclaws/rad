// Package protocol defines Rad's transport-neutral wire vocabulary: the
// rad:// connection URI, LIR payloads, and JSON values shared by clients and
// servers. It contains no HTTP routing, OpenAPI, or generated HTTP types;
// those live in package api. It has no engine imports, and LIR JSON validation
// is its only contract dependency outside the standard library.
//
// The LIR and PIR wire grammars are the Schemancer-generated union types in
// the lirwire and pirwire subpackages; this package owns their schema
// validation (ValidateLIRJSON, ValidatePIRJSON) and marshalling
// (MarshalQuery/UnmarshalQuery, MarshalProgram/UnmarshalProgram). There is no
// separate handwritten IR — the generated types are the one representation.
//
// # Connection URIs
//
//	rad://localhost            -> http://localhost:7237
//	rad://db.internal:9000     -> http://db.internal:9000
//	rads://db.example.com      -> https://db.example.com:7237
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
	"time"
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
	// Reason is the stable, fine-grained error identity within a code
	// (unknown_table, division_by_zero, …). It defaults to the code — the
	// class catch-all — when the source did not name a specific reason.
	Reason string `json:"reason"`
}

// NewProblem builds a Problem for a code, status, and detail message. Reason
// defaults to the code; use WithReason to name a specific one.
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
		Reason: code,
	}
}

// WithReason sets a specific reason, returning the updated Problem.
func (p Problem) WithReason(reason string) Problem {
	if reason != "" {
		p.Reason = reason
	}
	return p
}

// Record is one result row: scalar attributes plus nested relations
// (objects for first fields — null when absent — and arrays for array
// fields).
type Record = map[string]any

// DatabaseInfo is stable metadata about a Rad database.
type DatabaseInfo struct {
	Mode            string     `json:"mode"`
	SchemaVersion   uint64     `json:"schema_version"`
	SchemaHash      string     `json:"schema_hash"`
	SchemaVersionAt *time.Time `json:"schema_version_at,omitempty"`
}

// SchemaDocument is the transport mirror of the canonical catalog schema.
// The engine owns the canonical representation; API boundaries map it into
// this storage-free shape.
type SchemaDocument struct {
	Tables []TableDef `json:"tables,omitempty"`
}

type SchemaState struct {
	SchemaVersion uint64         `json:"schema_version"`
	SchemaHash    string         `json:"schema_hash"`
	Schema        SchemaDocument `json:"schema"`
}

type SchemaIdentity struct {
	SchemaVersion uint64 `json:"schema_version"`
	SchemaHash    string `json:"schema_hash"`
}

type SchemaChange struct {
	Kind    string `json:"kind"`
	Summary string `json:"summary"`
	Table   string `json:"table,omitempty"`
	Column  string `json:"column,omitempty"`
}

type SchemaFinding struct {
	Kind    string `json:"kind"`
	Summary string `json:"summary"`
	Table   string `json:"table,omitempty"`
	Column  string `json:"column,omitempty"`
	Rows    uint64 `json:"rows,omitempty"`
}

// SchemaDiff is an advisory point-in-time plan. Matching durable work may make
// Program omit a duplicate transition start. Apply replans transactionally and
// returns authoritative transition identities in SchemaMigration.
type SchemaDiff struct {
	CurrentVersion uint64          `json:"current_version"`
	CurrentHash    string          `json:"current_hash"`
	DesiredHash    string          `json:"desired_hash"`
	Changes        []SchemaChange  `json:"changes"`
	Program        any             `json:"program"`
	Destructive    []SchemaFinding `json:"destructive"`
	Blocking       []SchemaFinding `json:"blocking"`
}

type SchemaMigrationState string

const (
	SchemaMigrationConverging SchemaMigrationState = "converging"
	SchemaMigrationReady      SchemaMigrationState = "ready"
)

type SchemaMigration struct {
	SchemaState
	DesiredHash   string               `json:"desired_hash"`
	State         SchemaMigrationState `json:"state"`
	TransitionIDs []string             `json:"transition_ids"`
	Changes       []SchemaChange       `json:"changes"`
}

// TableInfo describes one table for introspection (GET /tables and the
// catalog mutation responses). It is the definition vocabulary read back:
// indexes and foreign keys reuse the def shapes, with foreign keys naming
// their referenced table rather than carrying internal IDs.
type TableInfo struct {
	ID          uint32          `json:"id"`
	Name        string          `json:"name"`
	Columns     []ColumnInfo    `json:"columns"`
	PrimaryKey  []string        `json:"primary_key"`
	Indexes     []IndexDef      `json:"indexes,omitempty"`
	ForeignKeys []ForeignKeyDef `json:"foreign_keys,omitempty"`
}

type ColumnInfo struct {
	ID       uint32         `json:"id"`
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
	ID          uint32          `json:"id,omitempty"`
	Name        string          `json:"name"`
	Columns     []ColumnDef     `json:"columns"`
	PrimaryKey  []string        `json:"primary_key"`
	Indexes     []IndexDef      `json:"indexes,omitempty"`
	ForeignKeys []ForeignKeyDef `json:"foreign_keys,omitempty"`
}

// ColumnDef defines one column. Type is one of text, int64, float64, bool.
type ColumnDef struct {
	ID       uint32         `json:"id,omitempty"`
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
