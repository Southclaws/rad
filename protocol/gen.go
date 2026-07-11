package protocol

// The wire types, HTTP client, and HTTP server for the Rad protocol are
// generated from openapi.yaml by ogen into the ./oas subpackage. The Query IR
// schemas live in lir.schema.json and are pulled in by reference; ogen.yaml
// enables that resolution. Regenerate with `go generate ./protocol` after
// editing either the contract or the IR schema.
//
//go:generate go run github.com/ogen-go/ogen/cmd/ogen@latest --config ogen.yaml --target ./oas --package oas --clean openapi.yaml
