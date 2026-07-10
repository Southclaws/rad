package main

// The Rad database API: an implementation of the ogen-generated oas.Handler
// (the OpenAPI contract in protocol/openapi.yaml). The generated server owns
// routing, request decoding, and response encoding; this file supplies the
// behaviour, translating the generated request types into engine calls and
// engine results and errors back into the contract's response shapes.
//
// HTTP concerns follow standard practice — panic recovery, request logging,
// body limits, timeouts, graceful shutdown — but there is deliberately no TLS
// (a reverse proxy terminates it) and no auth (out of MVP scope).

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"log"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/ogen-go/ogen/ogenerrors"

	"rad/protocol"
	"rad/protocol/oas"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	frontend "rad/rad/06_frontend"
)

const (
	maxBodyBytes  = 4 << 20 // schema files and batch payloads stay small
	txIdleTimeout = 60 * time.Second
)

// errTxNotFound marks a reference to a transaction that is unknown, expired,
// or already finished. It maps to a not_found problem on the wire.
var errTxNotFound = errors.New("unknown or expired transaction")

// dbAPI serves the wire protocol over one database. It implements oas.Handler.
type dbAPI struct {
	db  *frontend.DB
	cat *catalog.Catalog

	mu       sync.Mutex
	sessions map[string]*txSession
}

var _ oas.Handler = (*dbAPI)(nil)

// txSession is one server-held transaction. Requests within a session are
// serialized (transactions are not concurrency-safe).
type txSession struct {
	mu       sync.Mutex
	tx       *frontend.Tx
	lastUsed time.Time
	done     bool
}

func newDBAPI(db *frontend.DB, cat *catalog.Catalog) *dbAPI {
	a := &dbAPI{db: db, cat: cat, sessions: map[string]*txSession{}}
	go a.reapSessions()
	return a
}

// httpHandler builds the generated HTTP server, delegating any path that is
// not a contract route to notFound (the devtool UI and its API ride along on
// the same port).
func (a *dbAPI) httpHandler(notFound http.Handler) (http.Handler, error) {
	return oas.NewServer(a,
		oas.WithNotFound(notFound.ServeHTTP),
		oas.WithErrorHandler(problemErrorHandler),
	)
}

// view is the shared read/write surface of *frontend.DB and *frontend.Tx.
type view interface {
	Create(ctx context.Context, table string, row qir.Row) (qir.Row, error)
	Update(ctx context.Context, table string, key, set qir.Row) (qir.Row, bool, error)
	Delete(ctx context.Context, table string, key qir.Row) (bool, error)
	Get(ctx context.Context, table string, key qir.Row) (qir.Row, bool, error)
	Execute(ctx context.Context, q qir.Query) (qir.Datum, error)
}

// ── core operations (transport free) ─────────────────────────────────────
//
// Each runs one wire operation against a view, coercing wire values by the
// catalog's column types and returning wire-shaped results. They are shared
// verbatim by the autocommit and transaction handlers below.

func (a *dbAPI) doQuery(ctx context.Context, v view, wq protocol.Query) ([]protocol.Record, error) {
	q, err := graphQuery(wq)
	if err != nil {
		return nil, err
	}
	d, err := v.Execute(ctx, q)
	if err != nil {
		return nil, err
	}
	return datumRecords(d), nil
}

// datumRecords renders a result as the wire's record list: an array yields
// its objects, a single object (a root fold) yields one record.
func datumRecords(d qir.Datum) []protocol.Record {
	toRecord := func(el qir.Datum) protocol.Record {
		if m, ok := frontend.DatumJSON(el).(map[string]any); ok {
			return m
		}
		return protocol.Record{}
	}
	switch d.Kind {
	case qir.DatumArray:
		out := make([]protocol.Record, len(d.Elems))
		for i, el := range d.Elems {
			out[i] = toRecord(el)
		}
		return out
	case qir.DatumNull:
		return []protocol.Record{}
	}
	return []protocol.Record{toRecord(d)}
}

func (a *dbAPI) doCreate(ctx context.Context, v view, table string, values map[string]any) (protocol.Record, error) {
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(ctx, table)
	if err != nil {
		return nil, err
	}
	row, err := coerceRow(tbl, values)
	if err != nil {
		return nil, err
	}
	stored, err := v.Create(ctx, table, row)
	if err != nil {
		return nil, err
	}
	return rowJSON(stored), nil
}

func (a *dbAPI) doUpdate(ctx context.Context, v view, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(ctx, table)
	if err != nil {
		return nil, false, err
	}
	krow, err := coerceRow(tbl, key)
	if err != nil {
		return nil, false, err
	}
	srow, err := coerceRow(tbl, set)
	if err != nil {
		return nil, false, err
	}
	for _, name := range clear {
		col, ok := tbl.Column(name)
		if !ok {
			return nil, false, wireErrf("table %q has no column %q", tbl.Name, name)
		}
		srow[name] = qir.Null(col.Type)
	}
	stored, found, err := v.Update(ctx, table, krow, srow)
	if err != nil {
		return nil, false, err
	}
	return rowJSON(stored), found, nil
}

func (a *dbAPI) doDelete(ctx context.Context, v view, table string, key map[string]any) (bool, error) {
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(ctx, table)
	if err != nil {
		return false, err
	}
	krow, err := coerceRow(tbl, key)
	if err != nil {
		return false, err
	}
	return v.Delete(ctx, table, krow)
}

// ── meta and schema ───────────────────────────────────────────────────────

func (a *dbAPI) GetHealth(ctx context.Context) (*oas.Health, error) {
	return &oas.Health{Status: "ok"}, nil
}

func (a *dbAPI) TableList(ctx context.Context) (*oas.TableList, error) {
	tables, err := a.cat.ListTables(ctx)
	if err != nil {
		return nil, err
	}
	infos := make([]protocol.TableInfo, len(tables))
	for i, t := range tables {
		info := protocol.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			info.Columns = append(info.Columns, protocol.ColumnInfo{
				Name: c.Name, Type: string(c.Type), Nullable: c.Nullable, Format: c.Format,
			})
		}
		infos[i] = info
	}
	return &oas.TableList{Tables: protocol.TablesToOAS(infos)}, nil
}

func (a *dbAPI) SchemaMigrate(ctx context.Context, req oas.OptMigrateProps) (oas.SchemaMigrateRes, error) {
	steps, err := a.db.MigrateFile(ctx, "schema.rad", []byte(req.Or(oas.MigrateProps{}).Schema))
	if err != nil {
		if p := clientProblem(err); p != nil {
			op := protocol.ProblemToOAS(*p)
			return &op, nil
		}
		return nil, err
	}
	out := []string{}
	for _, s := range steps {
		out = append(out, s.String())
	}
	return &oas.MigrateResult{Steps: out}, nil
}

// ── autocommit data operations ────────────────────────────────────────────

func (a *dbAPI) Query(ctx context.Context, req oas.OptQuery) (oas.QueryRes, error) {
	recs, err := a.doQuery(ctx, a.db, protocol.QueryFromOAS(req.Or(oas.Query{})))
	if err != nil {
		if p := clientProblem(err); p != nil {
			op := protocol.ProblemToOAS(*p)
			return &op, nil
		}
		return nil, err
	}
	return &oas.QueryResult{Records: protocol.RecordsToOAS(recs)}, nil
}

func (a *dbAPI) RowCreate(ctx context.Context, req oas.OptRowCreateProps) (oas.RowCreateRes, error) {
	p := req.Or(oas.RowCreateProps{})
	rec, err := a.doCreate(ctx, a.db, p.Table, protocol.CellsToMap(p.Values))
	if err != nil {
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.RowCreateConflict)(&op), nil
			}
			return (*oas.RowCreateUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return recordResult(rec, true), nil
}

func (a *dbAPI) RowUpdate(ctx context.Context, req oas.OptRowUpdateProps) (oas.RowUpdateRes, error) {
	p := req.Or(oas.RowUpdateProps{})
	rec, found, err := a.doUpdate(ctx, a.db, p.Table, protocol.CellsToMap(p.Key), protocol.CellsToMap(p.Set), p.Clear)
	if err != nil {
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.RowUpdateConflict)(&op), nil
			}
			return (*oas.RowUpdateUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return recordResult(rec, found), nil
}

func (a *dbAPI) RowDelete(ctx context.Context, req oas.OptRowDeleteProps) (oas.RowDeleteRes, error) {
	p := req.Or(oas.RowDeleteProps{})
	found, err := a.doDelete(ctx, a.db, p.Table, protocol.CellsToMap(p.Key))
	if err != nil {
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.RowDeleteConflict)(&op), nil
			}
			return (*oas.RowDeleteUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return &oas.DeleteResult{Found: found}, nil
}

// ── transactions ──────────────────────────────────────────────────────────

func (a *dbAPI) TransactionBegin(ctx context.Context) (*oas.TransactionCredentials, error) {
	// Sessions outlive the request; use a background context.
	tx, err := a.db.Begin(context.Background())
	if err != nil {
		return nil, err
	}
	id := newSessionID()
	a.mu.Lock()
	a.sessions[id] = &txSession{tx: tx, lastUsed: time.Now()}
	a.mu.Unlock()
	return &oas.TransactionCredentials{ID: id}, nil
}

func (a *dbAPI) TransactionQuery(ctx context.Context, req oas.OptQuery, params oas.TransactionQueryParams) (oas.TransactionQueryRes, error) {
	var recs []protocol.Record
	err := a.withSession(params.ID, func(v view) error {
		var e error
		recs, e = a.doQuery(ctx, v, protocol.QueryFromOAS(req.Or(oas.Query{})))
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionQueryNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			return (*oas.TransactionQueryUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return &oas.QueryResult{Records: protocol.RecordsToOAS(recs)}, nil
}

func (a *dbAPI) TransactionRowCreate(ctx context.Context, req oas.OptRowCreateProps, params oas.TransactionRowCreateParams) (oas.TransactionRowCreateRes, error) {
	p := req.Or(oas.RowCreateProps{})
	var rec protocol.Record
	err := a.withSession(params.ID, func(v view) error {
		var e error
		rec, e = a.doCreate(ctx, v, p.Table, protocol.CellsToMap(p.Values))
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionRowCreateNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.TransactionRowCreateConflict)(&op), nil
			}
			return (*oas.TransactionRowCreateUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return recordResult(rec, true), nil
}

func (a *dbAPI) TransactionRowUpdate(ctx context.Context, req oas.OptRowUpdateProps, params oas.TransactionRowUpdateParams) (oas.TransactionRowUpdateRes, error) {
	p := req.Or(oas.RowUpdateProps{})
	var rec protocol.Record
	var found bool
	err := a.withSession(params.ID, func(v view) error {
		var e error
		rec, found, e = a.doUpdate(ctx, v, p.Table, protocol.CellsToMap(p.Key), protocol.CellsToMap(p.Set), p.Clear)
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionRowUpdateNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.TransactionRowUpdateConflict)(&op), nil
			}
			return (*oas.TransactionRowUpdateUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return recordResult(rec, found), nil
}

func (a *dbAPI) TransactionRowDelete(ctx context.Context, req oas.OptRowDeleteProps, params oas.TransactionRowDeleteParams) (oas.TransactionRowDeleteRes, error) {
	p := req.Or(oas.RowDeleteProps{})
	var found bool
	err := a.withSession(params.ID, func(v view) error {
		var e error
		found, e = a.doDelete(ctx, v, p.Table, protocol.CellsToMap(p.Key))
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionRowDeleteNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := protocol.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.TransactionRowDeleteConflict)(&op), nil
			}
			return (*oas.TransactionRowDeleteUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return &oas.DeleteResult{Found: found}, nil
}

func (a *dbAPI) TransactionCommit(ctx context.Context, params oas.TransactionCommitParams) (oas.TransactionCommitRes, error) {
	err := a.finish(params.ID, func(s *txSession) error { return s.tx.Commit(ctx) })
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionCommitNotFound)(&op), nil
		}
		if frontend.IsConflict(err) {
			op := protocol.ProblemToOAS(protocol.NewProblem(protocol.CodeConflict, http.StatusConflict, err.Error()))
			return (*oas.TransactionCommitConflict)(&op), nil
		}
		return nil, err
	}
	return &oas.NoContent{}, nil
}

func (a *dbAPI) TransactionRollback(ctx context.Context, params oas.TransactionRollbackParams) (oas.TransactionRollbackRes, error) {
	err := a.finish(params.ID, func(s *txSession) error { return s.tx.Rollback() })
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return &op, nil // a bare Problem on this operation encodes 404
		}
		return nil, err
	}
	return &oas.NoContent{}, nil
}

// ── session management ────────────────────────────────────────────────────

// withSession resolves and locks a transaction session, running fn against
// its view. It returns errTxNotFound if the id is unknown, expired, or
// already finished.
func (a *dbAPI) withSession(id string, fn func(v view) error) error {
	s, ok := a.session(id)
	if !ok {
		return errTxNotFound
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.done {
		return errTxNotFound
	}
	s.lastUsed = time.Now()
	return fn(s.tx)
}

// finish marks a session done, removes it, and runs the terminal operation
// (commit or rollback) under its lock.
func (a *dbAPI) finish(id string, fn func(*txSession) error) error {
	s, ok := a.session(id)
	if !ok {
		return errTxNotFound
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.done {
		return errTxNotFound
	}
	s.done = true
	a.mu.Lock()
	delete(a.sessions, id)
	a.mu.Unlock()
	return fn(s)
}

func (a *dbAPI) session(id string) (*txSession, bool) {
	a.mu.Lock()
	defer a.mu.Unlock()
	s, ok := a.sessions[id]
	return s, ok
}

// reapSessions rolls back transactions idle past txIdleTimeout — abandoned
// sessions must not pin SlateDB's conflict-tracking state forever.
func (a *dbAPI) reapSessions() {
	for range time.Tick(txIdleTimeout / 4) {
		cutoff := time.Now().Add(-txIdleTimeout)
		a.mu.Lock()
		var expired []*txSession
		for id, s := range a.sessions {
			if s.lastUsed.Before(cutoff) {
				expired = append(expired, s)
				delete(a.sessions, id)
			}
		}
		a.mu.Unlock()
		for _, s := range expired {
			s.mu.Lock()
			if !s.done {
				s.done = true
				_ = s.tx.Rollback()
			}
			s.mu.Unlock()
		}
	}
}

func newSessionID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b[:])
}

// ── error and result mapping ──────────────────────────────────────────────

// clientProblem classifies an engine or conversion error as a client-facing
// problem, or returns nil when it is an unexpected internal error that should
// surface as a 500 through NewError. Conflicts are retryable optimistic
// races; the rest are the caller's fault and safe to relay.
func clientProblem(err error) *protocol.Problem {
	var we wireErr
	switch {
	case errors.As(err, &we):
		p := protocol.NewProblem(protocol.CodeInvalid, http.StatusUnprocessableEntity, err.Error())
		return &p
	case frontend.IsConflict(err):
		p := protocol.NewProblem(protocol.CodeConflict, http.StatusConflict, err.Error())
		return &p
	case isEngineClientError(err):
		p := protocol.NewProblem(protocol.CodeInvalid, http.StatusUnprocessableEntity, err.Error())
		return &p
	default:
		return nil
	}
}

// isEngineClientError distinguishes constraint/validation failures (the
// caller's fault, safe to relay) from unexpected internals. The engine
// prefixes its own errors.
func isEngineClientError(err error) bool {
	msg := err.Error()
	for _, prefix := range []string{"exec:", "catalog:", "planner:", "migrate:", "schema.rad:"} {
		if strings.HasPrefix(msg, prefix) || strings.Contains(msg, ": "+prefix) {
			return true
		}
	}
	return false
}

// txNotFound builds the not_found problem for a spent or unknown transaction.
func txNotFound() oas.Problem {
	return protocol.ProblemToOAS(protocol.NewProblem(protocol.CodeNotFound, http.StatusNotFound, errTxNotFound.Error()))
}

// recordResult wraps a single-row result, omitting the record when absent.
func recordResult(rec protocol.Record, found bool) *oas.RecordResult {
	res := &oas.RecordResult{Found: found}
	if found {
		res.Record = oas.NewOptRecord(protocol.MapToRecord(rec))
	}
	return res
}

// NewError renders an unexpected internal error as the contract's default
// response. The detail is deliberately generic; the real error is logged.
func (a *dbAPI) NewError(ctx context.Context, err error) *oas.InternalServerErrorStatusCode {
	log.Printf("internal error: %v", err)
	return &oas.InternalServerErrorStatusCode{
		StatusCode: http.StatusInternalServerError,
		Response:   protocol.ProblemToOAS(protocol.NewProblem(protocol.CodeInternal, http.StatusInternalServerError, "internal error")),
	}
}

// problemErrorHandler renders request decoding and routing failures raised by
// the generated server as RFC 7807 problems, so even malformed requests get a
// contract-shaped body rather than ogen's plain text default.
func problemErrorHandler(ctx context.Context, w http.ResponseWriter, r *http.Request, err error) {
	status := ogenerrors.ErrorCode(err)
	w.Header().Set("Content-Type", protocol.ProblemContentType)
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(protocol.NewProblem(protocol.CodeInvalid, status, err.Error()))
}

func rowJSON(row qir.Row) protocol.Record {
	if row == nil {
		return nil
	}
	return frontend.RowJSON(row)
}

// ── middleware ──────────────────────────────────────────────────────────

func withRecovery(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if p := recover(); p != nil {
				log.Printf("panic serving %s %s: %v", r.Method, r.URL.Path, p)
				w.Header().Set("Content-Type", protocol.ProblemContentType)
				w.WriteHeader(http.StatusInternalServerError)
				_ = json.NewEncoder(w).Encode(protocol.NewProblem(protocol.CodeInternal, http.StatusInternalServerError, "internal error"))
			}
		}()
		next.ServeHTTP(w, r)
	})
}

// withBodyLimit caps request bodies. The generated server imposes no limit of
// its own on JSON payloads; an oversized body surfaces to the client as an
// invalid-request problem via the error handler.
func withBodyLimit(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Body != nil {
			r.Body = http.MaxBytesReader(w, r.Body, maxBodyBytes)
		}
		next.ServeHTTP(w, r)
	})
}

type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (s *statusRecorder) WriteHeader(code int) {
	s.status = code
	s.ResponseWriter.WriteHeader(code)
}

func withLogging(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		rec := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(rec, r)
		log.Printf("%s %s %d %s", r.Method, r.URL.Path, rec.status, time.Since(start).Round(time.Microsecond))
	})
}

// newHTTPServer applies the standard timeouts.
func newHTTPServer(addr string, handler http.Handler) *http.Server {
	return &http.Server{
		Addr:              addr,
		Handler:           handler,
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       30 * time.Second,
		WriteTimeout:      60 * time.Second,
		IdleTimeout:       120 * time.Second,
		MaxHeaderBytes:    1 << 20,
	}
}
