// Package api owns the OpenAPI-described HTTP surface. LIR and the shared
// wire vocabulary live separately in package protocol.
package api

// The wire types, HTTP client, and HTTP server for the Rad protocol are
// generated from openapi.yaml by ogen into the ./oas subpackage. The Query IR
// is a separate contract generated from lir.schema.json by Schemancer. Run
// `task protocol:generate` after editing either contract.
//
//go:generate go run github.com/ogen-go/ogen/cmd/ogen --config ogen.yaml --target ./oas --package oas --clean openapi.yaml
