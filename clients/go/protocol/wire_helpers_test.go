package protocol

// Shared construction helpers for the wire round-trip tests. The LIR/PIR wire
// types store union members as pointers, so these smooth over the pointer-field
// and marshalling boilerplate the tests need.

import (
	"encoding/json"
	"strconv"

	"github.com/Southclaws/rad/clients/go/protocol/lirwire"
)

// relBytes marshals a built LIR query into the opaque relation bytes a PIR
// statement carries.
func relBytes(q lirwire.Query) []byte { b, _ := json.Marshal(q); return b }

// mustValue formats a Go scalar as its canonical rows-cell lexeme, preserving
// json.Number precision; a nil scalar is a NULL cell.
func mustValue(v any) lirwire.Cell {
	switch x := v.(type) {
	case nil:
		return nil
	case string:
		return &x
	case json.Number:
		s := x.String()
		return &s
	case int:
		s := strconv.FormatInt(int64(x), 10)
		return &s
	case int64:
		s := strconv.FormatInt(x, 10)
		return &s
	case float64:
		s := strconv.FormatFloat(x, 'g', -1, 64)
		return &s
	case bool:
		s := strconv.FormatBool(x)
		return &s
	}
	return nil
}

func ptrBool(b bool) *bool                 { return &b }
func ptrInt(i int) *int                    { return &i }
func ptrExpr(e lirwire.Expr) *lirwire.Expr { return &e }
