package pgwire

import (
	"context"
	"strings"

	"github.com/jackc/pgx/v5/pgtype"
	wire "github.com/jeroenrinzema/psql-wire"
)

// Introspection stubs. Migration tooling (ent's Atlas engine) inspects
// pg_catalog/information_schema before diffing; this frontend answers that
// inspection with a fixed picture — one empty "public" schema — so the
// tooling always re-emits its full CREATE DDL, which the compiler then
// treats idempotently. Everything here matches on normalized query text,
// keeping catalog emulation out of the SQL compiler entirely.

type stubResult struct {
	columns []string
	rows    [][]any
	tag     string
}

func stubFor(query string) *wire.PreparedStatement {
	q := strings.ToLower(strings.TrimSpace(query))
	res := matchStub(q)
	if res == nil {
		return nil
	}
	cols := make(wire.Columns, len(res.columns))
	for i, name := range res.columns {
		cols[i] = wire.Column{Name: name, Oid: pgtype.TextOID, Width: -1}
	}
	handler := func(ctx context.Context, writer wire.DataWriter, _ []wire.Parameter) error {
		for _, row := range res.rows {
			if err := writer.Row(row); err != nil {
				return err
			}
		}
		return writer.Complete(res.tag)
	}
	// The stub still has to declare the query's $N placeholders (Atlas
	// inspection queries carry long IN lists) or drivers refuse to bind.
	return wire.NewStatement(handler,
		wire.WithColumns(cols),
		wire.WithParameters(wire.ParseParameters(query)),
	)
}

func matchStub(q string) *stubResult {
	if !strings.HasPrefix(q, "select") && !strings.HasPrefix(q, "with") {
		return nil
	}
	switch {
	// Atlas runtime parameters probe.
	case strings.Contains(q, "current_setting('server_version_num')"):
		return &stubResult{
			columns: []string{"current_setting", "current_setting", "current_setting"},
			rows:    [][]any{{"170000", "heap", nil}},
			tag:     "SELECT 1",
		}
	// Atlas schema listing: one empty public schema.
	case strings.Contains(q, "nspname as schema_name"):
		return &stubResult{
			columns: []string{"schema_name", "comment"},
			rows:    [][]any{{"public", nil}},
			tag:     "SELECT 1",
		}
	case strings.Contains(q, "version()") && !strings.Contains(q, " from "):
		return &stubResult{
			columns: []string{"version"},
			rows:    [][]any{{"PostgreSQL 17.0 (Rad)"}},
			tag:     "SELECT 1",
		}
	case strings.Contains(q, "current_schema") && !strings.Contains(q, " from "):
		return &stubResult{
			columns: []string{"current_schema"},
			rows:    [][]any{{"public"}},
			tag:     "SELECT 1",
		}
	case strings.Contains(q, "current_database") && !strings.Contains(q, " from "):
		return &stubResult{
			columns: []string{"current_database"},
			rows:    [][]any{{"rad"}},
			tag:     "SELECT 1",
		}
	// Any other system-catalog read: an empty result set.
	case strings.Contains(q, "pg_catalog.") ||
		strings.Contains(q, "information_schema.") ||
		strings.Contains(q, "from pg_") ||
		strings.Contains(q, "join pg_"):
		return &stubResult{tag: "SELECT 0"}
	}
	return nil
}
