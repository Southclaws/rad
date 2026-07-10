package main

// Coercion of wire cells (plain JSON values) into typed engine rows for the
// CRUD operations, driven by the catalog's column types: JSON numbers become
// int64 or float64 according to the column, never by guessing. Query
// lowering lives in qirconv.go.

import (
	"context"
	"encoding/json"
	"fmt"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// wireErr is a client-caused conversion error (HTTP 400).
type wireErr struct{ msg string }

func (e wireErr) Error() string { return e.msg }

func wireErrf(format string, args ...any) error {
	return wireErr{fmt.Sprintf(format, args...)}
}

// coerceValue converts a JSON value into a typed qir.Value for col.
func coerceValue(col catalog.Column, v any) (qir.Value, error) {
	if v == nil {
		return qir.Null(col.Type), nil
	}
	switch col.Type {
	case catalog.TypeText:
		s, ok := v.(string)
		if !ok {
			return qir.Value{}, wireErrf("column %q expects a string, got %T", col.Name, v)
		}
		return qir.Text(s), nil
	case catalog.TypeInt64:
		n, ok := v.(json.Number)
		if !ok {
			return qir.Value{}, wireErrf("column %q expects a number, got %T", col.Name, v)
		}
		i, err := n.Int64()
		if err != nil {
			return qir.Value{}, wireErrf("column %q expects an integer, got %v", col.Name, n)
		}
		return qir.Int64(i), nil
	case catalog.TypeFloat64:
		n, ok := v.(json.Number)
		if !ok {
			return qir.Value{}, wireErrf("column %q expects a number, got %T", col.Name, v)
		}
		f, err := n.Float64()
		if err != nil {
			return qir.Value{}, wireErrf("column %q expects a float, got %v", col.Name, n)
		}
		return qir.Float64(f), nil
	case catalog.TypeBool:
		b, ok := v.(bool)
		if !ok {
			return qir.Value{}, wireErrf("column %q expects a boolean, got %T", col.Name, v)
		}
		return qir.Bool(b), nil
	}
	return qir.Value{}, wireErrf("column %q has unsupported type %q", col.Name, col.Type)
}

// coerceRow converts a JSON object into a typed row for tbl.
func coerceRow(tbl catalog.Table, values map[string]any) (qir.Row, error) {
	row := make(qir.Row, len(values))
	for name, v := range values {
		col, ok := tbl.Column(name)
		if !ok {
			return nil, wireErrf("table %q has no column %q", tbl.Name, name)
		}
		val, err := coerceValue(col, v)
		if err != nil {
			return nil, err
		}
		row[name] = val
	}
	return row, nil
}

// wireConv resolves tables for cell coercion.
type wireConv struct {
	cat *catalog.Catalog
}

func (c *wireConv) table(ctx context.Context, name string) (catalog.Table, error) {
	tbl, ok, err := c.cat.GetTable(ctx, name)
	if err != nil {
		return catalog.Table{}, err
	}
	if !ok {
		return catalog.Table{}, wireErrf("table %q does not exist", name)
	}
	return tbl, nil
}
