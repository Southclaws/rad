// Package radclient is Rad's Go client runtime: a thin, dependency-free
// HTTP client for the rad:// protocol. Generated clients (rad generate)
// build on it; it can also be used directly for dynamic access.
//
//	c, err := radclient.Dial("rad://localhost")
//	recs, err := c.Query(ctx, protocol.Read{Table: "users"})
//
// Values decode with json.Number so int64 columns keep full precision.
package radclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"time"

	"rad/protocol"
)

// Client talks to one Rad server. It is safe for concurrent use.
type Client struct {
	base string
	http *http.Client
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
	c := &Client{base: base, http: &http.Client{Timeout: 30 * time.Second}}
	for _, o := range opts {
		o(c)
	}
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

// do POSTs (or GETs when body is nil and method says so) JSON and decodes
// the response with number preservation.
func (c *Client) do(ctx context.Context, method, path string, body, out any) error {
	var rdr io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return err
		}
		rdr = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, rdr)
	if err != nil {
		return err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	dec := json.NewDecoder(resp.Body)
	dec.UseNumber()
	if resp.StatusCode/100 != 2 {
		var pb protocol.Problem
		if err := dec.Decode(&pb); err != nil || pb.Code == "" {
			pb = protocol.NewProblem(protocol.CodeInternal, resp.StatusCode, resp.Status)
		}
		return &APIError{Problem: pb}
	}
	if out == nil {
		_, _ = io.Copy(io.Discard, resp.Body)
		return nil
	}
	return dec.Decode(out)
}

// Ping checks server liveness.
func (c *Client) Ping(ctx context.Context) error {
	return c.do(ctx, http.MethodGet, "/health", nil, &struct{}{})
}

// Tables fetches the database's table definitions.
func (c *Client) Tables(ctx context.Context) ([]protocol.TableInfo, error) {
	var resp struct {
		Tables []protocol.TableInfo `json:"tables"`
	}
	if err := c.do(ctx, http.MethodGet, "/tables", nil, &resp); err != nil {
		return nil, err
	}
	return resp.Tables, nil
}

// Migrate reconciles the server's database with a schema.rad source and
// returns the applied steps.
func (c *Client) Migrate(ctx context.Context, schemaSrc string) ([]string, error) {
	var resp protocol.MigrateResponse
	if err := c.do(ctx, http.MethodPost, "/migrate", protocol.MigrateRequest{Schema: schemaSrc}, &resp); err != nil {
		return nil, err
	}
	return resp.Steps, nil
}

// View is the read/write surface shared by the Client (autocommit) and Tx
// (transactional). Generated table handles operate on a View.
type View interface {
	Query(ctx context.Context, r protocol.Read) ([]protocol.Record, error)
	Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error)
	Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error)
	Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error)
	Delete(ctx context.Context, table string, key map[string]any) (bool, error)
}

var (
	_ View = (*Client)(nil)
	_ View = (*Tx)(nil)
)

func (c *Client) Query(ctx context.Context, r protocol.Read) ([]protocol.Record, error) {
	return query(ctx, c, "", r)
}

func (c *Client) Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error) {
	return get(ctx, c, "", table, key)
}

func (c *Client) Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error) {
	return create(ctx, c, "", table, values)
}

func (c *Client) Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	return update(ctx, c, "", table, key, set, clear)
}

func (c *Client) Delete(ctx context.Context, table string, key map[string]any) (bool, error) {
	return del(ctx, c, "", table, key)
}

// shared op implementations, parameterized by the path prefix ("" for
// autocommit, "/tx/{id}" inside a transaction).

func query(ctx context.Context, c *Client, prefix string, r protocol.Read) ([]protocol.Record, error) {
	var resp protocol.QueryResponse
	if err := c.do(ctx, http.MethodPost, prefix+"/query", r, &resp); err != nil {
		return nil, err
	}
	return resp.Records, nil
}

func get(ctx context.Context, c *Client, prefix, table string, key map[string]any) (protocol.Record, bool, error) {
	var resp protocol.RecordResponse
	if err := c.do(ctx, http.MethodPost, prefix+"/get", protocol.GetRequest{Table: table, Key: key}, &resp); err != nil {
		return nil, false, err
	}
	return resp.Record, resp.Found, nil
}

func create(ctx context.Context, c *Client, prefix, table string, values map[string]any) (protocol.Record, error) {
	var resp protocol.RecordResponse
	if err := c.do(ctx, http.MethodPost, prefix+"/create", protocol.CreateRequest{Table: table, Values: values}, &resp); err != nil {
		return nil, err
	}
	return resp.Record, nil
}

func update(ctx context.Context, c *Client, prefix, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	var resp protocol.RecordResponse
	req := protocol.UpdateRequest{Table: table, Key: key, Set: set, Clear: clear}
	if err := c.do(ctx, http.MethodPost, prefix+"/update", req, &resp); err != nil {
		return nil, false, err
	}
	return resp.Record, resp.Found, nil
}

func del(ctx context.Context, c *Client, prefix, table string, key map[string]any) (bool, error) {
	var resp protocol.DeleteResponse
	if err := c.do(ctx, http.MethodPost, prefix+"/delete", protocol.DeleteRequest{Table: table, Key: key}, &resp); err != nil {
		return false, err
	}
	return resp.Found, nil
}

// Tx is a server-held transaction session.
type Tx struct {
	c      *Client
	prefix string
}

func (t *Tx) Query(ctx context.Context, r protocol.Read) ([]protocol.Record, error) {
	return query(ctx, t.c, t.prefix, r)
}

func (t *Tx) Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error) {
	return get(ctx, t.c, t.prefix, table, key)
}

func (t *Tx) Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error) {
	return create(ctx, t.c, t.prefix, table, values)
}

func (t *Tx) Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	return update(ctx, t.c, t.prefix, table, key, set, clear)
}

func (t *Tx) Delete(ctx context.Context, table string, key map[string]any) (bool, error) {
	return del(ctx, t.c, t.prefix, table, key)
}

// Begin starts a transaction session. Prefer Txn.
func (c *Client) Begin(ctx context.Context) (*Tx, error) {
	var resp protocol.TxResponse
	if err := c.do(ctx, http.MethodPost, "/tx", struct{}{}, &resp); err != nil {
		return nil, err
	}
	return &Tx{c: c, prefix: "/tx/" + resp.ID}, nil
}

// Commit atomically applies the transaction's writes. A conflict error
// (IsConflict) means the transaction lost an optimistic race — retry it.
func (t *Tx) Commit(ctx context.Context) error {
	return t.c.do(ctx, http.MethodPost, t.prefix+"/commit", struct{}{}, nil)
}

// Rollback discards the transaction. Safe to call after Commit (the server
// treats a completed session as gone; the error is swallowed).
func (t *Tx) Rollback(ctx context.Context) error {
	err := t.c.do(ctx, http.MethodPost, t.prefix+"/rollback", struct{}{}, nil)
	var ae *APIError
	if errors.As(err, &ae) && (ae.Problem.Code == protocol.CodeNotFound || ae.Problem.Code == protocol.CodeInvalid) {
		return nil
	}
	return err
}

// Txn runs fn in a transaction and commits if fn returns nil, rolling back
// otherwise. Retry the whole Txn when IsConflict(err).
func (c *Client) Txn(ctx context.Context, fn func(tx *Tx) error) error {
	tx, err := c.Begin(ctx)
	if err != nil {
		return err
	}
	defer tx.Rollback(ctx)
	if err := fn(tx); err != nil {
		return err
	}
	return tx.Commit(ctx)
}
