package frontend

// The relation-graph read surface: Execute takes an unbound lir.Query — the
// form every frontend (the wire, future SQL/GraphQL) lowers into — and
// returns the typed result value tree. DatumJSON renders that tree as plain
// JSON-ready Go values: the database's result shape is nested, never a
// flattened join.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/06_frontend/resultjson"
)

// Execute runs a query against committed state.
func (db *DB) Execute(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return db.eng.Execute(ctx, q)
}

// ExecuteForced runs a query with every access forced to a full table scan.
// The result must match Execute — that equality is the planner's path-
// independence invariant, which a differential checks by running both.
func (db *DB) ExecuteForced(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return db.eng.ExecuteForced(ctx, q)
}

// Execute inside a transaction sees its snapshot plus its own writes.
func (tx *Tx) Execute(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return tx.tx.Execute(ctx, q)
}

// ExecuteJSON runs a query and renders the result.
func (db *DB) ExecuteJSON(ctx context.Context, q lir.Query) (any, error) {
	d, err := db.Execute(ctx, q)
	if err != nil {
		return nil, err
	}
	return resultjson.Datum(d), nil
}
