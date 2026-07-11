package main

// Coercion of wire cells (plain JSON values) into typed engine rows for the
// CRUD operations, driven by the catalog's column types: JSON numbers become
// int64 or float64 according to the column, never by guessing. Query
// lowering lives in lirconv.go.

import (
	"context"
	"encoding/json"
	"fmt"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// wireErr is a client-caused conversion error (HTTP 400).
type wireErr struct{ msg string }

func (e wireErr) Error() string { return e.msg }

func wireErrf(format string, args ...any) error {
	return wireErr{fmt.Sprintf(format, args...)}
}

// coerceValue converts a JSON value into a typed lir.Value for col.
func coerceValue(col catalog.Column, v any) (lir.Value, error) {
	if v == nil {
		return lir.Null(col.Type), nil
	}
	switch col.Type {
	case catalog.TypeText:
		s, ok := v.(string)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a string, got %T", col.Name, v)
		}
		return lir.Text(s), nil
	case catalog.TypeInt64:
		n, ok := v.(json.Number)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a number, got %T", col.Name, v)
		}
		i, err := n.Int64()
		if err != nil {
			return lir.Value{}, wireErrf("column %q expects an integer, got %v", col.Name, n)
		}
		return lir.Int64(i), nil
	case catalog.TypeFloat64:
		n, ok := v.(json.Number)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a number, got %T", col.Name, v)
		}
		f, err := n.Float64()
		if err != nil {
			return lir.Value{}, wireErrf("column %q expects a float, got %v", col.Name, n)
		}
		return lir.Float64(f), nil
	case catalog.TypeBool:
		b, ok := v.(bool)
		if !ok {
			return lir.Value{}, wireErrf("column %q expects a boolean, got %T", col.Name, v)
		}
		return lir.Bool(b), nil
	}
	return lir.Value{}, wireErrf("column %q has unsupported type %q", col.Name, col.Type)
}

// coerceRow converts a JSON object into a typed row for tbl.
func coerceRow(tbl catalog.Table, values map[string]any) (lir.Row, error) {
	row := make(lir.Row, len(values))
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
