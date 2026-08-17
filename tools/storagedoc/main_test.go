package main

import (
	"strings"
	"testing"
)

func TestRender(t *testing.T) {
	registry := []byte(`{
		"root_magic": "7237",
		"keyspaces": [
			{"name": "data", "tag": "01", "key": "table:physical_id(t) primary_key:tuple", "value": "record row_body", "invariants": []},
			{"name": "catalog_table", "tag": "12", "key": "table:physical_id(t)", "value": "json schema catalog/table.v1", "invariants": []}
		],
		"reserved": [{"tag": "00", "reason": "guards truncated keys"}]
	}`)
	documents := map[string][]byte{
		"catalog/table.v1": []byte(`
title: catalog/table.v1
description: One physical table.
type: object
properties:
  format:
    description: Payload serialization format discriminator.
    type: integer
    enum: [1]
  record:
    description: One physical table definition.
    type: object
    required: [id]
    properties:
      id:
        description: Permanent physical table identity.
        type: string
      columns:
        description: The table's columns.
        type: array
        items:
          description: One physical column.
          type: object
          required: [type]
          properties:
            type:
              description: Scalar type of every stored cell.
              type: string
              enum: [text, int64]
`),
	}

	result, err := render(registry, documents)
	if err != nil {
		t.Fatal(err)
	}
	got := string(result)
	for _, want := range []string{
		"title: Storage format",
		"`0x7237`",
		"| `01` | `data` |",
		"[`catalog/table.v1`](#catalogtablev1)",
		"Tag `00` is permanently reserved",
		"## `catalog/table.v1`",
		"| `id` | `string` | Yes | Permanent physical table identity. |",
		"| `columns[].type` | `text \\| int64` | Yes |",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("generated page does not contain %q\n%s", want, got)
		}
	}
}
