// Package api owns Rad's HTTP API contract and the conversions between its
// ogen-generated representations and the transport-neutral wire protocol.
package api

// Bridge between the ergonomic hand-written HTTP types in this package and
// the ogen-generated types in ./oas. LIR query conversion lives separately in
// lirjson.go because its JSON Schema is intentionally not part of OpenAPI.
//
// The conversions are near mechanical: Rad's request and response shapes were
// designed alongside the contract, so the only real work is unwrapping ogen's
// Opt* fields and moving column values between Go's `any` (numbers decoded as
// json.Number to keep int64 precision) and ogen's raw JSON (jx.Raw).

import (
	"bytes"
	"encoding/json"

	"github.com/go-faster/jx"
)

// ProblemContentType is the HTTP media type for RFC 7807 responses.
const ProblemContentType = "application/problem+json"

// rawToAny decodes a raw JSON value into a Go value, preserving numbers as
// json.Number so int64 columns keep full precision. An empty raw is null.
func rawToAny(raw jx.Raw) any {
	if len(raw) == 0 {
		return nil
	}
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		return nil
	}
	return v
}

// anyToRaw encodes a Go value as a raw JSON value. Column values are scalars
// or, for embedded relations, nested objects and arrays, all of which marshal
// cleanly; an encode failure yields JSON null rather than a broken document.
func anyToRaw(v any) jx.Raw {
	raw, err := json.Marshal(v)
	if err != nil {
		return jx.Raw("null")
	}
	return jx.Raw(raw)
}
