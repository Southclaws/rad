package main

// The RAD database API: the /v1 endpoints the client runtime speaks
// (protocol package). HTTP concerns follow standard practice — panic
// recovery, request logging, body limits, timeouts, graceful shutdown — but
// there is deliberately no TLS (a reverse proxy terminates it) and no auth
// (out of MVP scope).

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

	"rad/protocol"
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	frontend "rad/rad/06_frontend"
)

const (
	maxBodyBytes  = 4 << 20 // schema files and batch payloads stay small
	txIdleTimeout = 60 * time.Second
)

// dbAPI serves the /v1 protocol over one database.
type dbAPI struct {
	db  *frontend.DB
	cat *catalog.Catalog

	mu       sync.Mutex
	sessions map[string]*txSession
}

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

// view is the shared read/write surface of *frontend.DB and *frontend.Tx.
type view interface {
	Create(ctx context.Context, table string, row lir.Row) (lir.Row, error)
	Update(ctx context.Context, table string, key, set lir.Row) (lir.Row, bool, error)
	Delete(ctx context.Context, table string, key lir.Row) (bool, error)
	Get(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error)
	Read(ctx context.Context, r lir.Read) ([]*frontend.Record, error)
}

func (a *dbAPI) register(mux *http.ServeMux) {
	mux.HandleFunc("GET /v1/health", a.handleHealth)
	mux.HandleFunc("GET /v1/tables", a.handleTables)
	mux.HandleFunc("POST /v1/migrate", a.handleMigrate)

	mux.HandleFunc("POST /v1/query", a.direct(a.handleQuery))
	mux.HandleFunc("POST /v1/get", a.direct(a.handleGet))
	mux.HandleFunc("POST /v1/create", a.direct(a.handleCreate))
	mux.HandleFunc("POST /v1/update", a.direct(a.handleUpdate))
	mux.HandleFunc("POST /v1/delete", a.direct(a.handleDelete))

	mux.HandleFunc("POST /v1/tx", a.handleTxBegin)
	mux.HandleFunc("POST /v1/tx/{id}/query", a.inTx(a.handleQuery))
	mux.HandleFunc("POST /v1/tx/{id}/get", a.inTx(a.handleGet))
	mux.HandleFunc("POST /v1/tx/{id}/create", a.inTx(a.handleCreate))
	mux.HandleFunc("POST /v1/tx/{id}/update", a.inTx(a.handleUpdate))
	mux.HandleFunc("POST /v1/tx/{id}/delete", a.inTx(a.handleDelete))
	mux.HandleFunc("POST /v1/tx/{id}/commit", a.handleTxCommit)
	mux.HandleFunc("POST /v1/tx/{id}/rollback", a.handleTxRollback)
}

// direct runs an operation against the database's committed state.
func (a *dbAPI) direct(op func(http.ResponseWriter, *http.Request, view)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		op(w, r, a.db)
	}
}

// inTx runs an operation inside a held transaction session.
func (a *dbAPI) inTx(op func(http.ResponseWriter, *http.Request, view)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		s, ok := a.session(r.PathValue("id"))
		if !ok {
			writeError(w, http.StatusNotFound, protocol.CodeNotFound, "unknown or expired transaction")
			return
		}
		s.mu.Lock()
		defer s.mu.Unlock()
		if s.done {
			writeError(w, http.StatusConflict, protocol.CodeInvalid, "transaction already completed")
			return
		}
		s.lastUsed = time.Now()
		op(w, r, s.tx)
	}
}

// ── handlers ────────────────────────────────────────────────────────────

func (a *dbAPI) handleHealth(w http.ResponseWriter, r *http.Request) {
	writeJSONBody(w, http.StatusOK, map[string]string{"status": "ok"})
}

func (a *dbAPI) handleTables(w http.ResponseWriter, r *http.Request) {
	tables, err := a.cat.ListTables(r.Context())
	if err != nil {
		writeErr(w, err)
		return
	}
	out := make([]protocol.TableInfo, len(tables))
	for i, t := range tables {
		info := protocol.TableInfo{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			info.Columns = append(info.Columns, protocol.ColumnInfo{
				Name: c.Name, Type: string(c.Type), Nullable: c.Nullable, Format: c.Format,
			})
		}
		out[i] = info
	}
	writeJSONBody(w, http.StatusOK, map[string]any{"tables": out})
}

func (a *dbAPI) handleMigrate(w http.ResponseWriter, r *http.Request) {
	var req protocol.MigrateRequest
	if !readJSONBody(w, r, &req) {
		return
	}
	steps, err := a.db.MigrateFile(r.Context(), "schema.rad", []byte(req.Schema))
	if err != nil {
		writeErr(w, err)
		return
	}
	resp := protocol.MigrateResponse{Steps: []string{}}
	for _, s := range steps {
		resp.Steps = append(resp.Steps, s.String())
	}
	writeJSONBody(w, http.StatusOK, resp)
}

func (a *dbAPI) handleQuery(w http.ResponseWriter, r *http.Request, v view) {
	var req protocol.Read
	if !readJSONBody(w, r, &req) {
		return
	}
	conv := &wireConv{cat: a.cat}
	read, err := conv.toRead(r.Context(), req)
	if err != nil {
		writeErr(w, err)
		return
	}
	recs, err := v.Read(r.Context(), read)
	if err != nil {
		writeErr(w, err)
		return
	}
	rows := frontend.RecordsJSON(recs)
	if rows == nil {
		rows = []map[string]any{}
	}
	writeJSONBody(w, http.StatusOK, protocol.QueryResponse{Records: rows})
}

func (a *dbAPI) handleGet(w http.ResponseWriter, r *http.Request, v view) {
	var req protocol.GetRequest
	if !readJSONBody(w, r, &req) {
		return
	}
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(r.Context(), req.Table)
	if err != nil {
		writeErr(w, err)
		return
	}
	key, err := coerceRow(tbl, req.Key)
	if err != nil {
		writeErr(w, err)
		return
	}
	row, ok, err := v.Get(r.Context(), req.Table, key)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeRecord(w, rowJSON(row), ok)
}

func (a *dbAPI) handleCreate(w http.ResponseWriter, r *http.Request, v view) {
	var req protocol.CreateRequest
	if !readJSONBody(w, r, &req) {
		return
	}
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(r.Context(), req.Table)
	if err != nil {
		writeErr(w, err)
		return
	}
	values, err := coerceRow(tbl, req.Values)
	if err != nil {
		writeErr(w, err)
		return
	}
	stored, err := v.Create(r.Context(), req.Table, values)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeRecord(w, rowJSON(stored), true)
}

func (a *dbAPI) handleUpdate(w http.ResponseWriter, r *http.Request, v view) {
	var req protocol.UpdateRequest
	if !readJSONBody(w, r, &req) {
		return
	}
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(r.Context(), req.Table)
	if err != nil {
		writeErr(w, err)
		return
	}
	key, err := coerceRow(tbl, req.Key)
	if err != nil {
		writeErr(w, err)
		return
	}
	set, err := coerceRow(tbl, req.Set)
	if err != nil {
		writeErr(w, err)
		return
	}
	for _, name := range req.Clear {
		col, ok := tbl.Column(name)
		if !ok {
			writeErr(w, wireErrf("table %q has no column %q", tbl.Name, name))
			return
		}
		set[name] = lir.Null(col.Type)
	}
	stored, found, err := v.Update(r.Context(), req.Table, key, set)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeRecord(w, rowJSON(stored), found)
}

func (a *dbAPI) handleDelete(w http.ResponseWriter, r *http.Request, v view) {
	var req protocol.DeleteRequest
	if !readJSONBody(w, r, &req) {
		return
	}
	conv := &wireConv{cat: a.cat}
	tbl, err := conv.table(r.Context(), req.Table)
	if err != nil {
		writeErr(w, err)
		return
	}
	key, err := coerceRow(tbl, req.Key)
	if err != nil {
		writeErr(w, err)
		return
	}
	found, err := v.Delete(r.Context(), req.Table, key)
	if err != nil {
		writeErr(w, err)
		return
	}
	writeJSONBody(w, http.StatusOK, protocol.DeleteResponse{Found: found})
}

// ── transactions ────────────────────────────────────────────────────────

func (a *dbAPI) handleTxBegin(w http.ResponseWriter, r *http.Request) {
	// Sessions outlive the request; use a background context.
	tx, err := a.db.Begin(context.Background())
	if err != nil {
		writeErr(w, err)
		return
	}
	id := newSessionID()
	a.mu.Lock()
	a.sessions[id] = &txSession{tx: tx, lastUsed: time.Now()}
	a.mu.Unlock()
	writeJSONBody(w, http.StatusOK, protocol.TxResponse{ID: id})
}

func (a *dbAPI) handleTxCommit(w http.ResponseWriter, r *http.Request) {
	a.finishTx(w, r, func(s *txSession) error { return s.tx.Commit(r.Context()) })
}

func (a *dbAPI) handleTxRollback(w http.ResponseWriter, r *http.Request) {
	a.finishTx(w, r, func(s *txSession) error { return s.tx.Rollback() })
}

func (a *dbAPI) finishTx(w http.ResponseWriter, r *http.Request, fn func(*txSession) error) {
	id := r.PathValue("id")
	s, ok := a.session(id)
	if !ok {
		writeError(w, http.StatusNotFound, protocol.CodeNotFound, "unknown or expired transaction")
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.done {
		writeError(w, http.StatusConflict, protocol.CodeInvalid, "transaction already completed")
		return
	}
	s.done = true
	a.mu.Lock()
	delete(a.sessions, id)
	a.mu.Unlock()
	if err := fn(s); err != nil {
		writeErr(w, err)
		return
	}
	writeJSONBody(w, http.StatusOK, map[string]string{"status": "ok"})
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

// ── plumbing ────────────────────────────────────────────────────────────

// readJSONBody decodes a limited, number-preserving JSON body; on failure
// it writes the error response and returns false.
func readJSONBody(w http.ResponseWriter, r *http.Request, dst any) bool {
	if ct := r.Header.Get("Content-Type"); ct != "" && !strings.HasPrefix(ct, "application/json") {
		writeError(w, http.StatusUnsupportedMediaType, protocol.CodeInvalid, "Content-Type must be application/json")
		return false
	}
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxBodyBytes))
	dec.UseNumber()
	if err := dec.Decode(dst); err != nil {
		writeError(w, http.StatusBadRequest, protocol.CodeInvalid, "invalid JSON body: "+err.Error())
		return false
	}
	return true
}

func writeJSONBody(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeRecord(w http.ResponseWriter, rec protocol.Record, found bool) {
	resp := protocol.RecordResponse{Found: found}
	if found {
		resp.Record = rec
	}
	writeJSONBody(w, http.StatusOK, resp)
}

func writeError(w http.ResponseWriter, status int, code, msg string) {
	writeJSONBody(w, status, protocol.ErrorResponse{Code: code, Message: msg})
}

// writeErr maps engine and conversion errors onto protocol error codes.
func writeErr(w http.ResponseWriter, err error) {
	var we wireErr
	switch {
	case errors.As(err, &we):
		writeError(w, http.StatusBadRequest, protocol.CodeInvalid, err.Error())
	case frontend.IsConflict(err):
		writeError(w, http.StatusConflict, protocol.CodeConflict, err.Error())
	case isEngineClientError(err):
		writeError(w, http.StatusUnprocessableEntity, protocol.CodeInvalid, err.Error())
	default:
		log.Printf("internal error: %v", err)
		writeError(w, http.StatusInternalServerError, protocol.CodeInternal, "internal error")
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

// ── middleware ──────────────────────────────────────────────────────────

func withRecovery(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if p := recover(); p != nil {
				log.Printf("panic serving %s %s: %v", r.Method, r.URL.Path, p)
				writeError(w, http.StatusInternalServerError, protocol.CodeInternal, "internal error")
			}
		}()
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

func rowJSON(row lir.Row) protocol.Record {
	if row == nil {
		return nil
	}
	rec := &frontend.Record{Columns: row}
	return frontend.RecordsJSON([]*frontend.Record{rec})[0]
}
