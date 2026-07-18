package lir

import (
	"cmp"
	"fmt"
	"strings"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// Value is a typed SQL-ish scalar — the runtime datum of the IR. Exactly one
// of the payload fields is meaningful, selected by Type; Null overrides the
// payload.
//
// The JSON encoding of this struct is a STORAGE CONTRACT: rows persist as
// column-ID-keyed maps of these values (05_exec rowcodec), so the field tags
// must never change.
type Value struct {
	Type    model.Type `json:"type"`
	Text    string     `json:"text,omitempty"`
	Int64   int64      `json:"int64,omitempty"`
	Float64 float64    `json:"float64,omitempty"`
	Bool    bool       `json:"bool,omitempty"`
	Null    bool       `json:"null,omitempty"`
}

// Row maps column names (or alias-qualified names during query execution,
// e.g. "u.id") to values.
type Row map[string]Value

func Text(s string) Value     { return Value{Type: model.TypeText, Text: s} }
func Int64(i int64) Value     { return Value{Type: model.TypeInt64, Int64: i} }
func Float64(f float64) Value { return Value{Type: model.TypeFloat64, Float64: f} }
func Bool(b bool) Value       { return Value{Type: model.TypeBool, Bool: b} }
func Null(t model.Type) Value { return Value{Type: t, Null: true} }

// AppendIdentity appends the type-tagged scalar representation used for
// full-row set identity.
func (v Value) AppendIdentity(dst []byte) []byte {
	if v.Null {
		return append(dst, "|N"...)
	}
	dst = fmt.Appendf(dst, "|%s:", v.Type)
	switch v.Type {
	case model.TypeText:
		return fmt.Appendf(dst, "%d:%s", len(v.Text), v.Text)
	case model.TypeInt64:
		return fmt.Appendf(dst, "%d", v.Int64)
	case model.TypeFloat64:
		return fmt.Appendf(dst, "%v", v.Float64)
	case model.TypeBool:
		return fmt.Appendf(dst, "%t", v.Bool)
	}
	return dst
}

// Equal compares two values two-valued: nulls never equal anything,
// including each other. (Three-valued comparison lives in the bound
// expression evaluator; Equal serves storage-side checks like index
// maintenance, where a missing value can never match.)
func (v Value) Equal(o Value) bool {
	if v.Null || o.Null {
		return false
	}
	if v.Type != o.Type {
		return false
	}
	switch v.Type {
	case model.TypeText:
		return v.Text == o.Text
	case model.TypeInt64:
		return v.Int64 == o.Int64
	case model.TypeFloat64:
		return v.Float64 == o.Float64
	case model.TypeBool:
		return v.Bool == o.Bool
	}
	return false
}

// Compare orders v against o: -1, 0, or 1. NULL sorts before every value
// (matching the key encoding). Comparing different non-null types is an
// error.
func (v Value) Compare(o Value) (int, error) {
	switch {
	case v.Null && o.Null:
		return 0, nil
	case v.Null:
		return -1, nil
	case o.Null:
		return 1, nil
	}
	if v.Type != o.Type {
		return 0, fmt.Errorf("lir: cannot compare %s with %s", v.Type, o.Type)
	}
	switch v.Type {
	case model.TypeText:
		return strings.Compare(v.Text, o.Text), nil
	case model.TypeInt64:
		return cmp.Compare(v.Int64, o.Int64), nil
	case model.TypeFloat64:
		return cmp.Compare(v.Float64, o.Float64), nil
	case model.TypeBool:
		switch {
		case v.Bool == o.Bool:
			return 0, nil
		case o.Bool:
			return -1, nil
		default:
			return 1, nil
		}
	}
	return 0, fmt.Errorf("lir: cannot compare type %s", v.Type)
}

func (v Value) String() string {
	if v.Null {
		return "NULL"
	}
	switch v.Type {
	case model.TypeText:
		return fmt.Sprintf("%q", v.Text)
	case model.TypeInt64:
		return fmt.Sprintf("%d", v.Int64)
	case model.TypeFloat64:
		return fmt.Sprintf("%g", v.Float64)
	case model.TypeBool:
		return fmt.Sprintf("%t", v.Bool)
	}
	return fmt.Sprintf("<%s?>", v.Type)
}
