// Package api owns the OpenAPI-described HTTP surface. LIR and the shared
// wire vocabulary live separately in package protocol.
package api

// The wire types, HTTP client, and HTTP server for the Rad protocol are
// generated from the normative ../../../api/openapi.yaml document by ogen into
// the ./oas subpackage. LIR and PIR are separate contracts generated from the
// normative root schemas by Schemancer. Run `task generate:protocol` after
// editing LIR or PIR.
//
//go:generate go run github.com/ogen-go/ogen/cmd/ogen --config ogen.yaml --target ./oas --package oas --clean ../../../api/openapi.yaml
