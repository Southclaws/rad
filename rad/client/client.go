// Package radclient is Rad's Go client runtime: a thin client for the rad://
// protocol built on the ogen-generated transport in rad/api/oas. Generated
// clients (rad generate) build on it; it can also be used directly for
// dynamic access.
//
//	c, err := radclient.Dial("rad://localhost")
//	recs, err := c.Query(ctx, usersQuery) // a lirwire.Query graph
//
// The generated layer speaks the OpenAPI contract on the wire; this runtime
// builds LIR/PIR through the lirwire and pirwire builders and converts at the
// boundary, so callers never touch the generated Opt* wrappers or response
// unions. Values decode with json.Number so int64 columns keep full precision.
package radclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// Client talks to one Rad server. It is safe for concurrent use.
type Client struct {
	http   *http.Client
	oas    *oas.Client
	schema schemaCache
	compat schemaCompatibility
}

type schemaCompatibility struct {
	mu      sync.Mutex
	version uint64
	hash    string
	checked bool
	enabled bool
}

// Option configures a Client.
type Option func(*Client)

// WithHTTPClient substitutes the underlying *http.Client (custom transport,
// proxies, tracing).
func WithHTTPClient(h *http.Client) Option {
	return func(c *Client) { c.http = h }
}

// WithTimeout sets the per-request timeout (default 30s).
func WithTimeout(d time.Duration) Option {
	return func(c *Client) { c.http.Timeout = d }
}

// Dial parses a rad(s):// URI and returns a client. No connection is
// established until the first request; use Ping to verify reachability.
func Dial(rawurl string, opts ...Option) (*Client, error) {
	base, err := protocol.ParseURL(rawurl)
	if err != nil {
		return nil, err
	}
	c := &Client{http: &http.Client{Timeout: 30 * time.Second}}
	for _, o := range opts {
		o(c)
	}
	oc, err := oas.NewClient(base, oas.WithClient(c.http))
	if err != nil {
		return nil, err
	}
	c.oas = oc
	return c, nil
}

// APIError is a non-2xx response from the server, carrying its RFC 7807
// Problem Details.
type APIError struct {
	Problem protocol.Problem
}

func (e *APIError) Error() string {
	return fmt.Sprintf("rad: %s (%s, http %d)", e.Problem.Detail, e.Problem.Code, e.Problem.Status)
}

// IsConflict reports whether err is an optimistic transaction conflict —
// the standard remedy is retrying the whole transaction.
func IsConflict(err error) bool {
	var ae *APIError
	return errors.As(err, &ae) && ae.Problem.Code == protocol.CodeConflict
}

// IsNotFound reports whether err is a not_found error.
func IsNotFound(err error) bool {
	var ae *APIError
	return errors.As(err, &ae) && ae.Problem.Code == protocol.CodeNotFound
}

// apiError wraps a Problem-shaped response variant as an *APIError.
func apiError(p oas.Problem) error {
	return &APIError{Problem: api.ProblemFromOAS(p)}
}

// transportError maps errors returned by the generated client. The contract's
// default (500) response surfaces as *InternalServerErrorStatusCode and
// carries a Problem; anything else is a transport or decode failure.
func transportError(err error) error {
	if err == nil {
		return nil
	}
	var se *oas.InternalServerErrorStatusCode
	if errors.As(err, &se) {
		return &APIError{Problem: api.ProblemFromOAS(se.Response)}
	}
	return err
}

// Ping checks server liveness.
func (c *Client) Ping(ctx context.Context) error {
	if err := c.ensureSchema(ctx); err != nil {
		return err
	}
	_, err := c.oas.GetHealth(ctx)
	return transportError(err)
}

// ExpectSchema configures a one-time server compatibility check before the
// first generated-client operation. Direct runtime clients leave it unset.
func (c *Client) ExpectSchema(version uint64, hash string) {
	c.compat.mu.Lock()
	defer c.compat.mu.Unlock()
	c.compat.version = version
	c.compat.hash = hash
	c.compat.checked = false
	c.compat.enabled = true
}

func (c *Client) ensureSchema(ctx context.Context) error {
	c.compat.mu.Lock()
	defer c.compat.mu.Unlock()
	if !c.compat.enabled || c.compat.checked {
		return nil
	}
	if err := c.CheckSchema(ctx, c.compat.version, c.compat.hash); err != nil {
		return err
	}
	c.compat.checked = true
	return nil
}

// Tables fetches the database's table definitions.
func (c *Client) Tables(ctx context.Context) ([]protocol.TableInfo, error) {
	if err := c.ensureSchema(ctx); err != nil {
		return nil, err
	}
	res, err := c.oas.TableList(ctx)
	if err != nil {
		return nil, transportError(err)
	}
	return api.TablesFromOAS(res.Tables), nil
}

func decodeRawValue(raw oas.Value) any {
	if len(raw) == 0 {
		return nil
	}
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil
	}
	return value
}

// View is the read/write surface generated table handles operate on. Every
// operation is one PIR program over /execute; there is one implementation,
// the Client (autocommit). Multi-write atomicity is expressed by submitting a
// multi-statement program directly, not by a held session.
type View interface {
	Query(ctx context.Context, q lirwire.Query) ([]protocol.Record, error)
	QueryDatum(ctx context.Context, q lirwire.Query) (any, error)
	Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error)
	Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error)
	Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error)
	Delete(ctx context.Context, table string, key map[string]any) (bool, error)
}

var _ View = (*Client)(nil)

// Query runs a query and returns its result as records: an array result
// verbatim, an object result (first / exactly_one roots) as one record, a
// null result as none. A scalar root has no record shape — use QueryDatum.
func (c *Client) Query(ctx context.Context, q lirwire.Query) ([]protocol.Record, error) {
	return c.execQuery(ctx, q)
}

// QueryDatum runs a query and returns the result datum exactly as the root
// materialised: []any for many, map[string]any or nil for first/exactly_one,
// a naked value or nil for scalar. Numbers decode as json.Number.
func (c *Client) QueryDatum(ctx context.Context, q lirwire.Query) (any, error) {
	return c.execQueryDatum(ctx, q)
}

func (c *Client) Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error) {
	return c.execGet(ctx, table, key)
}

func (c *Client) Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error) {
	return c.execCreate(ctx, table, values)
}

func (c *Client) Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	return c.execUpdate(ctx, table, key, set, clear)
}

func (c *Client) Delete(ctx context.Context, table string, key map[string]any) (bool, error) {
	return c.execDelete(ctx, table, key)
}

// shared op implementations, parameterized by the transaction id ("" for
// autocommit, an opaque id inside a transaction). The autocommit and
// transaction operations are behaviourally identical; the only differences
// are the endpoint and the extra not_found the transaction form returns when
// its id is unknown or expired.

// decodeResult decodes the response's raw result datum with json.Number so
// int64 values keep full precision.
func decodeResult(raw oas.Value) (any, error) {
	if len(raw) == 0 {
		return nil, nil
	}
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	var d any
	if err := dec.Decode(&d); err != nil {
		return nil, fmt.Errorf("rad: decode result datum: %w", err)
	}
	return d, nil
}

// datumRecords views a result datum as records. A scalar root's naked value
// has no record shape and needs QueryDatum.
func datumRecords(d any) ([]protocol.Record, error) {
	switch v := d.(type) {
	case nil:
		return nil, nil
	case []any:
		recs := make([]protocol.Record, len(v))
		for i, el := range v {
			rec, ok := el.(map[string]any)
			if !ok {
				return nil, fmt.Errorf("rad: result element %d is %T, not a record", i, el)
			}
			recs[i] = rec
		}
		return recs, nil
	case map[string]any:
		return []protocol.Record{v}, nil
	default:
		return nil, fmt.Errorf("rad: result is a scalar (%T) — use QueryDatum for scalar-rooted queries", v)
	}
}
