// Package pgwire serves the PostgreSQL wire protocol in front of the
// engine: psql-wire handles the protocol state machines, rad/sql compiles
// each statement to a PIR program, and execution goes straight through the
// in-process engine handle — the same path /execute takes, minus HTTP.
package pgwire

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"strings"
	"time"

	wire "github.com/jeroenrinzema/psql-wire"
	"github.com/jeroenrinzema/psql-wire/codes"
	psqlerr "github.com/jeroenrinzema/psql-wire/errors"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/engine/06_frontend/resultjson"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/server/api"
	"github.com/Southclaws/rad/rad/sql"
)

type Server struct {
	db     *frontend.DB
	cat    *catalog.Catalog
	policy execprogram.CatalogPolicy
	wire   *wire.Server
	log    *slog.Logger
}

func New(db *frontend.DB, cat *catalog.Catalog, mode model.Mode, log *slog.Logger) (*Server, error) {
	s := &Server{
		db:     db,
		cat:    cat,
		policy: execprogram.CatalogForbidden,
		log:    log,
	}
	if mode == model.ModeDirect {
		s.policy = execprogram.CatalogRevisionPerStatement
	}
	srv, err := wire.NewServer(s.parse,
		wire.Version("17.0"),
		wire.GlobalParameters(wire.Parameters{
			"server_version":              "17.0",
			"server_encoding":             "UTF8",
			"client_encoding":             "UTF8",
			"standard_conforming_strings": "on",
			"integer_datetimes":           "on",
			"TimeZone":                    "UTC",
		}),
		wire.Logger(log),
	)
	if err != nil {
		return nil, err
	}
	s.wire = srv
	return s, nil
}

func (s *Server) ListenAndServe(addr string) error {
	return s.wire.ListenAndServe(addr)
}

func (s *Server) Serve(l net.Listener) error {
	return s.wire.Serve(l)
}

func (s *Server) Shutdown(ctx context.Context) error {
	return s.wire.Shutdown(ctx)
}

func (s *Server) parse(ctx context.Context, query string) (wire.PreparedStatements, error) {
	if st := stubFor(query); st != nil {
		return wire.Prepared(st), nil
	}
	stmts, err := sql.Parse(query)
	if err != nil {
		return nil, psqlerr.WithCode(err, codes.SyntaxErrorOrAccessRuleViolation)
	}
	if len(stmts) == 0 {
		// Comment-only input (pgx pings with "-- ping"): succeed doing
		// nothing.
		noop := func(ctx context.Context, writer wire.DataWriter, _ []wire.Parameter) error {
			return writer.Complete("")
		}
		return wire.Prepared(wire.NewStatement(noop)), nil
	}
	schema, err := s.snapshot(ctx)
	if err != nil {
		return nil, sqlstate(err)
	}
	prepared := make([]*wire.PreparedStatement, 0, len(stmts))
	for _, stmt := range stmts {
		p, err := sql.Prepare(schema, stmt)
		if err != nil {
			s.log.Debug("pgwire prepare failed", "query", truncate(query, 200), "error", err)
			return nil, sqlstate(err)
		}
		prepared = append(prepared, s.statement(p))
	}
	return prepared, nil
}

// snapshot reads the catalog into the compiler's schema form. Rebuilt per
// Parse so DDL is immediately visible; catalog reads are memory-cheap.
func (s *Server) snapshot(ctx context.Context) (*sql.Schema, error) {
	tables, err := s.cat.ListTables(ctx)
	if err != nil {
		return nil, err
	}
	infos := make([]protocol.TableInfo, 0, len(tables))
	for _, t := range tables {
		info := protocol.TableInfo{
			ID:         uint32(t.SchemaID),
			Name:       t.Name,
			PrimaryKey: t.PrimaryKey,
		}
		for _, col := range t.Columns {
			ci := protocol.ColumnInfo{
				ID:       uint32(col.SchemaID),
				Name:     col.Name,
				Type:     string(col.Type),
				Nullable: col.Nullable,
				Format:   col.Format,
			}
			if col.Default != nil {
				ci.Default = &protocol.ColumnDefault{Func: string(col.Default.Func)}
			}
			info.Columns = append(info.Columns, ci)
		}
		for _, ix := range t.Indexes {
			info.Indexes = append(info.Indexes, protocol.IndexDef{
				Name: ix.Name, Columns: ix.Columns, Unique: ix.Unique,
			})
		}
		infos = append(infos, info)
	}
	return sql.NewSchema(infos)
}

func (s *Server) statement(p *sql.Prepared) *wire.PreparedStatement {
	handler := func(ctx context.Context, writer wire.DataWriter, params []wire.Parameter) error {
		args, err := decodeArgs(p.Params, params)
		if err != nil {
			return sqlstate(err)
		}
		compiled, err := p.Compile(args)
		if err != nil {
			return sqlstate(err)
		}
		return s.execute(ctx, writer, compiled)
	}
	return wire.NewStatement(handler,
		wire.WithColumns(wireColumns(p.Columns)),
		wire.WithParameters(paramOIDs(p.Params)),
	)
}

func (s *Server) execute(ctx context.Context, writer wire.DataWriter, c *sql.Compiled) error {
	if c.Static != nil {
		for _, row := range c.Static {
			if err := writer.Row(row); err != nil {
				return err
			}
		}
		return writer.Complete(c.Tag)
	}
	if c.Program == nil {
		return writer.Complete(c.Tag)
	}

	engineProg, err := api.ProgramToEngine(*c.Program)
	if err != nil {
		return sqlstate(err)
	}
	var res execprogram.Result
	if len(engineProg.Statements) == 1 && engineProg.Statements[0].Kind == execprogram.Query {
		// A pure read runs against committed state on the dedicated read
		// path: no write intents, no serializable-conflict exposure.
		datum, err := s.db.Execute(ctx, engineProg.Statements[0].Rel)
		if err != nil {
			s.log.Debug("pgwire execute failed", "tag", c.Tag, "error", err)
			return sqlstate(err)
		}
		res = execprogram.Result{Result: datum}
	} else {
		// A SQL statement is one atomic program, so a lost serializable
		// race can be replayed invisibly — the behavior clients expect from
		// Postgres's default isolation, where single-statement writes never
		// surface serialization failures.
		for attempt := 0; ; attempt++ {
			res, err = s.db.ExecuteProgram(ctx, engineProg, execprogram.Options{Catalog: s.policy})
			if err == nil || attempt >= 50 || !isConflict(err) {
				break
			}
			backoff := time.Duration(1+attempt*attempt) * time.Millisecond
			if backoff > 40*time.Millisecond {
				backoff = 40 * time.Millisecond
			}
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(backoff):
			}
		}
	}
	if err != nil {
		// Concurrent migration runs race their CREATEs; the loser's
		// duplicate is as good as success.
		if c.DDL && strings.Contains(err.Error(), "exists") {
			return writer.Complete(c.Tag)
		}
		s.log.Debug("pgwire execute failed", "tag", c.Tag, "error", err)
		return sqlstate(err)
	}

	written := 0
	if len(c.Columns) > 0 {
		rows := datumRows(resultjson.Datum(res.Result))
		for _, row := range rows {
			out, err := encodeRow(c.Columns, row)
			if err != nil {
				return sqlstate(err)
			}
			if err := writer.Row(out); err != nil {
				return err
			}
			written++
		}
	}
	return writer.Complete(commandTag(c, res, written))
}

// commandTag renders the CommandComplete tag: row-returning statements
// count rows sent, mutations count affected rows across their statements.
func commandTag(c *sql.Compiled, res execprogram.Result, written int) string {
	switch c.Tag {
	case "SELECT", "SHOW":
		return fmt.Sprintf("SELECT %d", written)
	case "INSERT 0", "UPDATE", "DELETE", "TRUNCATE TABLE":
		affected := 0
		for _, st := range res.Statements {
			for _, name := range c.TagStmts {
				if st.Name == name {
					affected += st.Affected
				}
			}
		}
		if c.Tag == "TRUNCATE TABLE" {
			return "TRUNCATE TABLE"
		}
		return fmt.Sprintf("%s %d", c.Tag, affected)
	}
	return c.Tag
}

// datumRows flattens a result datum into row objects: an array yields its
// object elements, a lone object is one row, null is none.
func datumRows(v any) []map[string]any {
	switch t := v.(type) {
	case nil:
		return nil
	case []any:
		out := make([]map[string]any, 0, len(t))
		for _, el := range t {
			if m, ok := el.(map[string]any); ok {
				out = append(out, m)
			}
		}
		return out
	case map[string]any:
		return []map[string]any{t}
	}
	return nil
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
