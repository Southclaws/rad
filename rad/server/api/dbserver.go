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
	"fmt"
	"math"
	"net/http"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
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
}

var _ oas.Handler = (*dbAPI)(nil)

func newDBAPI(db *frontend.DB, cat *catalog.Catalog, mode catalog.Mode, location string) *dbAPI {
	return &dbAPI{db: db, cat: cat, mode: mode, location: location}
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

func (a *dbAPI) GetHealth(ctx context.Context) (*oas.Health, error) {
	return &oas.Health{Status: "ok"}, nil
}

func (a *dbAPI) GetInfo(ctx context.Context) (*oas.DatabaseInfo, error) {
	revision, err := a.cat.Revision(ctx)
	if err != nil {
		return nil, err
	}
	if revision.Version > math.MaxInt64 {
		return nil, fmt.Errorf("catalog: schema version %d exceeds the wire format", revision.Version)
	}
	info := &oas.DatabaseInfo{
		Mode:          oas.DatabaseInfoMode(a.mode),
		SchemaVersion: int64(revision.Version),
	}
	if !revision.CreatedAt.IsZero() {
		info.SchemaVersionAt = oas.NewOptDateTime(revision.CreatedAt)
	}
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
