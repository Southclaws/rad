package main

import (
	"strings"
	"testing"
)

func TestRender(t *testing.T) {
	source := []byte(`
title: Test LIR
description: A test query language.
$ref: "#/$defs/Query"
$defs:
  Query:
    description: A query.
    type: object
    required: [root]
    properties:
      root:
        $ref: "#/$defs/Root"
  Root:
    description: The selected root.
    type: object
    properties:
      cardinality:
        type: string
        enum: [many, first]
        default: many
  Node:
    description: A relation node.
    oneOf:
      - { $ref: "#/$defs/ScanNode" }
  ScanNode:
    description: Scan a table.
    type: object
    required: [kind, table]
    properties:
      kind: { type: string, const: scan }
      table: { type: string, minLength: 1 }
`)

	result, err := render(source, "lir")
	if err != nil {
		t.Fatal(err)
	}
	got := string(result)
	for _, want := range []string{
		"title: LIR schema",
		"## Query document",
		"#### `Query`",
		"| `root` | `Root` | Yes |",
		"## Relations",
		"**Variants:** `ScanNode`",
		"| `kind` | `scan` | Yes |",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("generated page does not contain %q\n%s", want, got)
		}
	}
}
