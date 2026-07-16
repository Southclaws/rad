package api

import (
	"fmt"
	"math"
	"strconv"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// Decoding wire scalars into the engine's untyped literal form. Two wire
// shapes carry a scalar — a self-describing Value (a literal) and a
// schema-directed Cell (a rows payload, typed by its column) — but both reduce
// to one lexeme-plus-type decode. The result is a Go scalar the binder already
// understands (int64/float64/string/bool, or nil for NULL); the binder types
// it against the context it meets. Numbers are the only lossy JSON scalars, so
// they travel as strings and are parsed here, where out-of-range int64 and
// non-finite float64 — bounds the wire schema cannot express — are rejected.

// decodeScalar decodes one lexeme as the given scalar type.
func decodeScalar(kind lirwire.ScalarType, lexeme string) (any, error) {
	switch kind {
	case lirwire.ScalarTypeText:
		return lexeme, nil
	case lirwire.ScalarTypeInt64:
		n, err := strconv.ParseInt(lexeme, 10, 64)
		if err != nil {
			return nil, fmt.Errorf("int64 value %q is malformed or out of the signed 64-bit range", lexeme)
		}
		return n, nil
	case lirwire.ScalarTypeFloat64:
		f, err := strconv.ParseFloat(lexeme, 64)
		if err != nil {
			return nil, fmt.Errorf("float64 value %q is malformed", lexeme)
		}
		if math.IsNaN(f) || math.IsInf(f, 0) {
			return nil, fmt.Errorf("float64 value %q is not finite", lexeme)
		}
		return f, nil
	case lirwire.ScalarTypeBool:
		switch lexeme {
		case "true":
			return true, nil
		case "false":
			return false, nil
		}
		return nil, fmt.Errorf("bool value %q must be \"true\" or \"false\"", lexeme)
	}
	return nil, fmt.Errorf("unknown scalar type %q", kind)
}

// decodeCell decodes one rows cell against its column's scalar type. A nil cell
// is a typed NULL.
func decodeCell(kind lirwire.ScalarType, cell lirwire.Cell) (any, error) {
	if cell == nil {
		return nil, nil
	}
	return decodeScalar(kind, *cell)
}

// decodeValue decodes a self-describing literal Value: its variant names the
// scalar type and carries the lexeme; an absent payload is a NULL. The NULL's
// declared type is not carried into the engine — the binder retypes a NULL
// from the context it meets (a naked NULL with no context is rejected there).
func decodeValue(v lirwire.Value) (any, error) {
	switch x := v.ValueUnion.(type) {
	case *lirwire.TextValue:
		if x.Value == nil {
			return nil, nil
		}
		return decodeScalar(lirwire.ScalarTypeText, *x.Value)
	case *lirwire.Int64Value:
		if x.Value == nil {
			return nil, nil
		}
		return decodeScalar(lirwire.ScalarTypeInt64, *x.Value)
	case *lirwire.Float64Value:
		if x.Value == nil {
			return nil, nil
		}
		return decodeScalar(lirwire.ScalarTypeFloat64, *x.Value)
	case *lirwire.BoolValue:
		if x.Value == nil {
			return nil, nil
		}
		return *x.Value, nil
	case nil:
		return nil, wireErrf("literal carries no value")
	}
	return nil, wireErrf("unknown value variant %T", v.ValueUnion)
}
