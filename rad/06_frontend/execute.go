package frontend

// The relation-graph read surface: Execute takes an unbound qir.Query — the
// form every frontend (the wire, future SQL/GraphQL) lowers into — and
// returns the typed result value tree. DatumJSON renders that tree as plain
// JSON-ready Go values: the database's result shape is nested, never a
// flattened join.

import (
	"context"
	"encoding/json"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// Execute runs a query against committed state.
func (db *DB) Execute(ctx context.Context, q qir.Query) (qir.Datum, error) {
	return db.eng.Execute(ctx, q)
}

// Execute inside a transaction sees its snapshot plus its own writes.
func (tx *Tx) Execute(ctx context.Context, q qir.Query) (qir.Datum, error) {
	return tx.tx.Execute(ctx, q)
}

// ExecuteJSON runs a query and renders the result.
func (db *DB) ExecuteJSON(ctx context.Context, q qir.Query) (any, error) {
	d, err := db.Execute(ctx, q)
	if err != nil {
		return nil, err
	}
	return DatumJSON(d), nil
}

// DatumJSON renders a result tree as JSON-ready values: objects become
// maps (an absent to-one relation is a PRESENT key with a nil value),
// arrays become slices (empty, never nil), scalars become native Go values,
// NULL becomes nil.
func DatumJSON(d qir.Datum) any {
	switch d.Kind {
	case qir.DatumScalar:
		return plainScalar(d.Scalar)
	case qir.DatumObject:
		m := make(map[string]any, len(d.Fields))
		for _, f := range d.Fields {
			m[f.Name] = DatumJSON(f.Datum)
		}
		return m
	case qir.DatumArray:
		out := make([]any, len(d.Elems))
		for i, e := range d.Elems {
			out[i] = DatumJSON(e)
		}
		return out
	default: // null
		return nil
	}
}

// RowJSON renders one stored row as a JSON-ready map — the shape mutation
// responses carry.
func RowJSON(row qir.Row) map[string]any {
	m := make(map[string]any, len(row))
	for name, v := range row {
		m[name] = plainScalar(v)
	}
	return m
}

// MarshalDatum renders a result as indented JSON with deterministic key
// order.
func MarshalDatum(d qir.Datum) ([]byte, error) {
	return json.MarshalIndent(DatumJSON(d), "", "  ")
}

func plainScalar(v qir.Value) any {
	if v.Null {
		return nil
	}
	switch v.Type {
	case catalog.TypeText:
		return v.Text
	case catalog.TypeInt64:
		return v.Int64
	case catalog.TypeFloat64:
		return v.Float64
	case catalog.TypeBool:
		return v.Bool
	}
	return nil
}
