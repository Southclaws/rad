// Package radclient is Rad's Go client runtime: a thin client for the rad://
// protocol built on the ogen-generated transport in protocol/oas. Generated
// clients (rad generate) build on it; it can also be used directly for
// dynamic access.
//
//	c, err := radclient.Dial("rad://localhost")
//	recs, err := c.Query(ctx, usersQuery) // a protocol.Query graph
//
// The generated layer speaks the OpenAPI contract on the wire; this runtime
// keeps the ergonomic protocol.* vocabulary and converts at the boundary, so
// callers never touch the generated Opt* wrappers or response unions. Values
// decode with json.Number so int64 columns keep full precision.
package radclient

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"time"

	"rad/protocol"
	"rad/protocol/oas"
)

// Client talks to one Rad server. It is safe for concurrent use.
type Client struct {
	http *http.Client
	oas  *oas.Client
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
	return &APIError{Problem: protocol.ProblemFromOAS(p)}
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
		return &APIError{Problem: protocol.ProblemFromOAS(se.Response)}
	}
	return err
}

// Ping checks server liveness.
func (c *Client) Ping(ctx context.Context) error {
	_, err := c.oas.GetHealth(ctx)
	return transportError(err)
}

// Tables fetches the database's table definitions.
func (c *Client) Tables(ctx context.Context) ([]protocol.TableInfo, error) {
	res, err := c.oas.TableList(ctx)
	if err != nil {
		return nil, transportError(err)
	}
	return protocol.TablesFromOAS(res.Tables), nil
}

// Migrate reconciles the server's database with a schema.rad source and
// returns the applied steps.
func (c *Client) Migrate(ctx context.Context, schemaSrc string) ([]string, error) {
	res, err := c.oas.SchemaMigrate(ctx, oas.NewOptMigrateProps(oas.MigrateProps{Schema: schemaSrc}))
	if err != nil {
		return nil, transportError(err)
	}
	switch v := res.(type) {
	case *oas.MigrateResult:
		return v.Steps, nil
	case *oas.Problem:
		return nil, apiError(*v)
	default:
		return nil, fmt.Errorf("rad: unexpected migrate response %T", res)
	}
}

// View is the read/write surface shared by the Client (autocommit) and Tx
// (transactional). Generated table handles operate on a View.
type View interface {
	Query(ctx context.Context, q protocol.Query) ([]protocol.Record, error)
	Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error)
	Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error)
	Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error)
	Delete(ctx context.Context, table string, key map[string]any) (bool, error)
}

var (
	_ View = (*Client)(nil)
	_ View = (*Tx)(nil)
)

func (c *Client) Query(ctx context.Context, q protocol.Query) ([]protocol.Record, error) {
	return query(ctx, c, "", q)
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

// shared op implementations, parameterized by the transaction id ("" for
// autocommit, an opaque id inside a transaction). The autocommit and
// transaction operations are behaviourally identical; the only differences
// are the endpoint and the extra not_found the transaction form returns when
// its id is unknown or expired.

func query(ctx context.Context, c *Client, txID string, q protocol.Query) ([]protocol.Record, error) {
	req := oas.NewOptQuery(protocol.QueryToOAS(q))
	if txID == "" {
		res, err := c.oas.Query(ctx, req)
		if err != nil {
			return nil, transportError(err)
		}
		switch v := res.(type) {
		case *oas.QueryResult:
			return protocol.RecordsFromOAS(v.Records), nil
		case *oas.Problem:
			return nil, apiError(*v)
		}
		return nil, fmt.Errorf("rad: unexpected query response %T", res)
	}
	res, err := c.oas.TransactionQuery(ctx, req, oas.TransactionQueryParams{ID: txID})
	if err != nil {
		return nil, transportError(err)
	}
	switch v := res.(type) {
	case *oas.QueryResult:
		return protocol.RecordsFromOAS(v.Records), nil
	case *oas.TransactionQueryNotFound:
		return nil, apiError(oas.Problem(*v))
	case *oas.TransactionQueryUnprocessableEntity:
		return nil, apiError(oas.Problem(*v))
	}
	return nil, fmt.Errorf("rad: unexpected query response %T", res)
}

// get fetches one row by primary key. There is no dedicated wire endpoint: a
// point read is a three-node graph — scan, filter on the key columns, slice
// of one — riding the same operation the fluent query builder uses.
func get(ctx context.Context, c *Client, txID, table string, key map[string]any) (protocol.Record, bool, error) {
	preds := make([]*protocol.Expr, 0, len(key))
	for col, val := range key {
		preds = append(preds, protocol.Eq(protocol.Col("s", col), protocol.Lit(val)))
	}
	recs, err := query(ctx, c, txID, protocol.Query{
		Nodes: map[string]protocol.Node{
			"s":    {Kind: "scan", Table: table, Scope: "s"},
			"keyed": {Kind: "filter", Input: "s", Predicate: protocol.AndAll(preds)},
			"one":  {Kind: "slice", Input: "keyed", Limit: new(int(1))},
		},
		Root: protocol.Root{Node: "one", Cardinality: "many"},
	})
	if err != nil {
		return nil, false, err
	}
	if len(recs) == 0 {
		return nil, false, nil
	}
	return recs[0], true, nil
}

func create(ctx context.Context, c *Client, txID, table string, values map[string]any) (protocol.Record, error) {
	req := oas.NewOptRowCreateProps(oas.RowCreateProps{Table: table, Values: protocol.MapToCells(values)})
	if txID == "" {
		res, err := c.oas.RowCreate(ctx, req)
		if err != nil {
			return nil, transportError(err)
		}
		switch v := res.(type) {
		case *oas.RecordResult:
			rec, _, e := record(v)
			return rec, e
		case *oas.RowCreateConflict:
			return nil, apiError(oas.Problem(*v))
		case *oas.RowCreateUnprocessableEntity:
			return nil, apiError(oas.Problem(*v))
		}
		return nil, fmt.Errorf("rad: unexpected create response %T", res)
	}
	res, err := c.oas.TransactionRowCreate(ctx, req, oas.TransactionRowCreateParams{ID: txID})
	if err != nil {
		return nil, transportError(err)
	}
	switch v := res.(type) {
	case *oas.RecordResult:
		rec, _, e := record(v)
		return rec, e
	case *oas.TransactionRowCreateConflict:
		return nil, apiError(oas.Problem(*v))
	case *oas.TransactionRowCreateNotFound:
		return nil, apiError(oas.Problem(*v))
	case *oas.TransactionRowCreateUnprocessableEntity:
		return nil, apiError(oas.Problem(*v))
	}
	return nil, fmt.Errorf("rad: unexpected create response %T", res)
}

func update(ctx context.Context, c *Client, txID, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	req := oas.NewOptRowUpdateProps(oas.RowUpdateProps{
		Table: table, Key: protocol.MapToCells(key), Set: protocol.MapToCells(set), Clear: clear,
	})
	if txID == "" {
		res, err := c.oas.RowUpdate(ctx, req)
		if err != nil {
			return nil, false, transportError(err)
		}
		switch v := res.(type) {
		case *oas.RecordResult:
			return record(v)
		case *oas.RowUpdateConflict:
			return nil, false, apiError(oas.Problem(*v))
		case *oas.RowUpdateUnprocessableEntity:
			return nil, false, apiError(oas.Problem(*v))
		}
		return nil, false, fmt.Errorf("rad: unexpected update response %T", res)
	}
	res, err := c.oas.TransactionRowUpdate(ctx, req, oas.TransactionRowUpdateParams{ID: txID})
	if err != nil {
		return nil, false, transportError(err)
	}
	switch v := res.(type) {
	case *oas.RecordResult:
		return record(v)
	case *oas.TransactionRowUpdateConflict:
		return nil, false, apiError(oas.Problem(*v))
	case *oas.TransactionRowUpdateNotFound:
		return nil, false, apiError(oas.Problem(*v))
	case *oas.TransactionRowUpdateUnprocessableEntity:
		return nil, false, apiError(oas.Problem(*v))
	}
	return nil, false, fmt.Errorf("rad: unexpected update response %T", res)
}

func del(ctx context.Context, c *Client, txID, table string, key map[string]any) (bool, error) {
	req := oas.NewOptRowDeleteProps(oas.RowDeleteProps{Table: table, Key: protocol.MapToCells(key)})
	if txID == "" {
		res, err := c.oas.RowDelete(ctx, req)
		if err != nil {
			return false, transportError(err)
		}
		switch v := res.(type) {
		case *oas.DeleteResult:
			return v.Found, nil
		case *oas.RowDeleteConflict:
			return false, apiError(oas.Problem(*v))
		case *oas.RowDeleteUnprocessableEntity:
			return false, apiError(oas.Problem(*v))
		}
		return false, fmt.Errorf("rad: unexpected delete response %T", res)
	}
	res, err := c.oas.TransactionRowDelete(ctx, req, oas.TransactionRowDeleteParams{ID: txID})
	if err != nil {
		return false, transportError(err)
	}
	switch v := res.(type) {
	case *oas.DeleteResult:
		return v.Found, nil
	case *oas.TransactionRowDeleteConflict:
		return false, apiError(oas.Problem(*v))
	case *oas.TransactionRowDeleteNotFound:
		return false, apiError(oas.Problem(*v))
	case *oas.TransactionRowDeleteUnprocessableEntity:
		return false, apiError(oas.Problem(*v))
	}
	return false, fmt.Errorf("rad: unexpected delete response %T", res)
}

// record unpacks a RecordResult into the wire record and its found flag.
func record(v *oas.RecordResult) (protocol.Record, bool, error) {
	if !v.Found {
		return nil, false, nil
	}
	return protocol.RecordToMap(v.Record.Or(nil)), true, nil
}

// Tx is a server-held transaction session.
type Tx struct {
	c  *Client
	id string
}

func (t *Tx) Query(ctx context.Context, q protocol.Query) ([]protocol.Record, error) {
	return query(ctx, t.c, t.id, q)
}

func (t *Tx) Get(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error) {
	return get(ctx, t.c, t.id, table, key)
}

func (t *Tx) Create(ctx context.Context, table string, values map[string]any) (protocol.Record, error) {
	return create(ctx, t.c, t.id, table, values)
}

func (t *Tx) Update(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	return update(ctx, t.c, t.id, table, key, set, clear)
}

func (t *Tx) Delete(ctx context.Context, table string, key map[string]any) (bool, error) {
	return del(ctx, t.c, t.id, table, key)
}

// Begin starts a transaction session. Prefer Txn.
func (c *Client) Begin(ctx context.Context) (*Tx, error) {
	res, err := c.oas.TransactionBegin(ctx)
	if err != nil {
		return nil, transportError(err)
	}
	return &Tx{c: c, id: res.ID}, nil
}

// Commit atomically applies the transaction's writes. A conflict error
// (IsConflict) means the transaction lost an optimistic race — retry it.
func (t *Tx) Commit(ctx context.Context) error {
	res, err := t.c.oas.TransactionCommit(ctx, oas.TransactionCommitParams{ID: t.id})
	if err != nil {
		return transportError(err)
	}
	switch v := res.(type) {
	case *oas.NoContent:
		return nil
	case *oas.TransactionCommitConflict:
		return apiError(oas.Problem(*v))
	case *oas.TransactionCommitNotFound:
		return apiError(oas.Problem(*v))
	}
	return fmt.Errorf("rad: unexpected commit response %T", res)
}

// Rollback discards the transaction. Safe to call after Commit (the server
// treats a completed session as gone, so a not_found here is success).
func (t *Tx) Rollback(ctx context.Context) error {
	_, err := t.c.oas.TransactionRollback(ctx, oas.TransactionRollbackParams{ID: t.id})
	if err != nil {
		err = transportError(err)
		var ae *APIError
		if errors.As(err, &ae) && (ae.Problem.Code == protocol.CodeNotFound || ae.Problem.Code == protocol.CodeInvalid) {
			return nil
		}
		return err
	}
	// The response is NoContent on success, or a Problem when the transaction
	// is already gone, which is also success for a defensive rollback.
	return nil
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
