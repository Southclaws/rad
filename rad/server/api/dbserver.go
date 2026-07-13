package api

// The Rad database API: an implementation of the ogen-generated oas.Handler
// (the OpenAPI contract in api/openapi.yaml). The generated server owns
// routing, request decoding, and response encoding; this file supplies the
// behaviour, translating the generated request types into engine calls and
// engine results and errors back into the contract's response shapes.
//
// HTTP concerns follow standard practice — panic recovery, request logging,
// body limits, timeouts, graceful shutdown — but there is deliberately no TLS
// (a reverse proxy terminates it) and no auth (out of MVP scope).

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/protocol"
)

// dbAPI serves the wire protocol over one database. It implements oas.Handler.
type dbAPI struct {
	db       *frontend.DB
	cat      *catalog.Catalog
	location string
	// mode is read once at construction: the catalog management mode is set
	// when the database is initialised and never changes, so caching it
	// keeps the gate on the imperative catalog operations free.
	mode catalog.Mode

	mu       sync.Mutex
	sessions map[string]*txSession
}

var _ oas.Handler = (*dbAPI)(nil)

func newDBAPI(db *frontend.DB, cat *catalog.Catalog, mode catalog.Mode, location string) *dbAPI {
	a := &dbAPI{db: db, cat: cat, mode: mode, location: location, sessions: map[string]*txSession{}}
	go a.reapSessions()
	return a
}

// httpHandler builds the generated HTTP server. Unmatched paths fall through
// to notFound; New passes a plain 404, since the admin UI is a separate
// server on its own port.
func (a *dbAPI) httpHandler(notFound http.Handler) (http.Handler, error) {
	return oas.NewServer(a,
		oas.WithNotFound(notFound.ServeHTTP),
		oas.WithErrorHandler(problemErrorHandler),
	)
}

// view is the shared read/write surface of *frontend.DB and *frontend.Tx.
type view interface {
	Create(ctx context.Context, table string, row lir.Row) (lir.Row, error)
	Update(ctx context.Context, table string, key, set lir.Row) (lir.Row, bool, error)
	Delete(ctx context.Context, table string, key lir.Row) (bool, error)
	Get(ctx context.Context, table string, key lir.Row) (lir.Row, bool, error)
	Execute(ctx context.Context, q lir.Query) (lir.Datum, error)
}

// doQuery executes a query and returns its result datum as raw JSON — the
// response carries one datum shaped exactly as the root materialised, so a
// scalar root is a naked value, not a smuggled empty record.
func (a *dbAPI) doQuery(ctx context.Context, v view, wq protocol.Query) (oas.Value, error) {
	q, err := graphQuery(wq)
	if err != nil {
		return nil, err
	}
	d, err := v.Execute(ctx, q)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(frontend.DatumJSON(d))
	if err != nil {
		return nil, fmt.Errorf("encode result datum: %w", err)
	}
	return oas.Value(raw), nil
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
		srow[name] = lir.Null(col.Type)
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

func (a *dbAPI) GetHealth(ctx context.Context) (*oas.Health, error) {
	return &oas.Health{Status: "ok", Mode: string(a.mode)}, nil
}

func (a *dbAPI) GetInfo(ctx context.Context) (*oas.DatabaseInfo, error) {
	info := &oas.DatabaseInfo{Mode: oas.DatabaseInfoMode(a.mode)}
	if a.location != "" {
		info.Location = oas.NewOptString(a.location)
	}
	return info, nil
}

func (a *dbAPI) TableList(ctx context.Context) (*oas.TableList, error) {
	tables, err := a.cat.ListTables(ctx)
	if err != nil {
		return nil, err
	}
	infos := make([]protocol.TableInfo, len(tables))
	for i, t := range tables {
		info, err := a.tableInfo(ctx, t)
		if err != nil {
			return nil, err
		}
		infos[i] = info
	}
	return &oas.TableList{Tables: api.TablesToOAS(infos)}, nil
}

func (a *dbAPI) SchemaMigrate(ctx context.Context, req oas.OptMigrateProps) (oas.SchemaMigrateRes, error) {
	steps, err := a.db.MigrateFile(ctx, "schema.rad", []byte(req.Or(oas.MigrateProps{}).Schema))
	if err != nil {
		if p := clientProblem(err); p != nil {
			op := api.ProblemToOAS(*p)
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

func (a *dbAPI) Query(ctx context.Context, req oas.Query) (oas.QueryRes, error) {
	q, err := protocol.UnmarshalQuery(req)
	if err != nil {
		op := api.ProblemToOAS(protocol.NewProblem(protocol.CodeInvalid, http.StatusBadRequest, err.Error()))
		return (*oas.QueryBadRequest)(&op), nil
	}
	result, err := a.doQuery(ctx, a.db, q)
	if err != nil {
		if p := clientProblem(err); p != nil {
			op := api.ProblemToOAS(*p)
			return (*oas.QueryUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return &oas.QueryResult{Result: result}, nil
}

func (a *dbAPI) RowCreate(ctx context.Context, req oas.OptRowCreateProps) (oas.RowCreateRes, error) {
	p := req.Or(oas.RowCreateProps{})
	rec, err := a.doCreate(ctx, a.db, p.Table, api.CellsToMap(p.Values))
	if err != nil {
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
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
	rec, found, err := a.doUpdate(ctx, a.db, p.Table, api.CellsToMap(p.Key), api.CellsToMap(p.Set), p.Clear)
	if err != nil {
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
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
	found, err := a.doDelete(ctx, a.db, p.Table, api.CellsToMap(p.Key))
	if err != nil {
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
			if cp.Code == protocol.CodeConflict {
				return (*oas.RowDeleteConflict)(&op), nil
			}
			return (*oas.RowDeleteUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return &oas.DeleteResult{Found: found}, nil
}

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

func (a *dbAPI) TransactionQuery(ctx context.Context, req oas.Query, params oas.TransactionQueryParams) (oas.TransactionQueryRes, error) {
	q, err := protocol.UnmarshalQuery(req)
	if err != nil {
		op := api.ProblemToOAS(protocol.NewProblem(protocol.CodeInvalid, http.StatusBadRequest, err.Error()))
		return (*oas.TransactionQueryBadRequest)(&op), nil
	}
	var result oas.Value
	err = a.withSession(params.ID, func(v view) error {
		var e error
		result, e = a.doQuery(ctx, v, q)
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionQueryNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
			return (*oas.TransactionQueryUnprocessableEntity)(&op), nil
		}
		return nil, err
	}
	return &oas.QueryResult{Result: result}, nil
}

func (a *dbAPI) TransactionRowCreate(ctx context.Context, req oas.OptRowCreateProps, params oas.TransactionRowCreateParams) (oas.TransactionRowCreateRes, error) {
	p := req.Or(oas.RowCreateProps{})
	var rec protocol.Record
	err := a.withSession(params.ID, func(v view) error {
		var e error
		rec, e = a.doCreate(ctx, v, p.Table, api.CellsToMap(p.Values))
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionRowCreateNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
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
		rec, found, e = a.doUpdate(ctx, v, p.Table, api.CellsToMap(p.Key), api.CellsToMap(p.Set), p.Clear)
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionRowUpdateNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
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
		found, e = a.doDelete(ctx, v, p.Table, api.CellsToMap(p.Key))
		return e
	})
	if err != nil {
		if errors.Is(err, errTxNotFound) {
			op := txNotFound()
			return (*oas.TransactionRowDeleteNotFound)(&op), nil
		}
		if cp := clientProblem(err); cp != nil {
			op := api.ProblemToOAS(*cp)
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
			op := api.ProblemToOAS(protocol.NewProblem(protocol.CodeConflict, http.StatusConflict, err.Error()))
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

// New builds the wire-protocol HTTP handler: the generated OpenAPI database
// API. Unmatched routes return 404; the parent server wraps it with shared
// middleware and pairs it with the admin UI on a separate port. The catalog
// management mode is read once here — it is set at database initialisation
// and immutable, so the imperative catalog operations gate on a cached value.
func New(db *frontend.DB, cat *catalog.Catalog, locations ...string) (http.Handler, error) {
	mode, err := cat.Mode(context.Background())
	if err != nil {
		return nil, err
	}
	location := ""
	if len(locations) > 0 {
		location = locations[0]
	}
	return newDBAPI(db, cat, mode, location).httpHandler(http.NotFoundHandler())
}
