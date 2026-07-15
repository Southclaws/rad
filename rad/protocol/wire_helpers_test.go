package protocol

// Shared construction helpers for the wire round-trip tests. The LIR/PIR wire
// types store union members as pointers and carry raw-JSON Values, so these
// smooth over the pointer-field and marshalling boilerplate the tests need.

import (
	"encoding/json"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// relBytes marshals a built LIR query into the opaque relation bytes a PIR
// statement carries.
func relBytes(q lirwire.Query) []byte { b, _ := json.Marshal(q); return b }

// mustValue encodes a Go scalar as a raw-JSON wire Value, preserving
// json.Number precision.
func mustValue(v any) lirwire.Value { val, _ := lirwire.SetAny(v); return val }

func ptrBool(b bool) *bool                 { return &b }
func ptrInt(i int) *int                    { return &i }
func ptrExpr(e lirwire.Expr) *lirwire.Expr { return &e }
