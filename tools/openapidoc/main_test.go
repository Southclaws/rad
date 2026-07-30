package main

import (
	"strings"
	"testing"
)

func TestRender(t *testing.T) {
	source := []byte(`
openapi: 3.0.3
info:
  title: rad
  version: v0
  description: Rad over HTTP.
tags:
  - name: data
    description: Read and write data.
paths:
  /things/{thing}:
    get:
      operationId: ThingGet
      summary: Read a thing.
      tags: [data]
      parameters:
        - name: thing
          in: path
          required: true
          schema: { type: string }
      responses:
        "200":
          description: The thing.
          content:
            application/json:
              schema: { $ref: "#/components/schemas/Thing" }
components:
  schemas:
    Thing:
      type: object
      required: [name]
      properties:
        name:
          type: string
          description: Its name.
`)

	result, err := render(source)
	if err != nil {
		t.Fatal(err)
	}
	got := string(result)
	for _, want := range []string{
		"### Data",
		"#### `GET` `/things/{thing}`",
		"| `thing` | `path` | `string` | Yes |",
		"| `200` | application/json `Thing` | The thing. |",
		"#### `Thing`",
		"| `name` | `string` | Yes | Its name. |",
	} {
		if !strings.Contains(got, want) {
			t.Errorf("generated page does not contain %q\n%s", want, got)
		}
	}
}
