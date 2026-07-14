package protocol

// A program survives the PIR wire codec with its statements, LIR relations,
// and int64 precision intact, and validation enforces the envelope, the
// program semantics (unique names, result selection), and each statement's
// LIR payload through the independent LIR schema.

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
)

// oneRow is a one-row constant relation, the simplest valid LIR document.
func oneRow(col string, v any) Query {
	return Query{
		Nodes: map[string]Node{
			"r": {Kind: "rows", Scope: "r",
				Columns: []RowsColumn{{Name: col, Type: "int64"}},
				Rows:    [][]any{{v}}},
		},
		Root: Root{Node: "r", Cardinality: "many"},
	}
}

func TestProgramRoundTrip(t *testing.T) {
	big := json.Number("9007199254740993") // > 2^53
	p := Program{
		Statements: []Statement{
			Create("author", "users", Query{
				Nodes: map[string]Node{
					"r": {Kind: "rows", Scope: "r",
						Columns: []RowsColumn{{Name: "id", Type: "int64"}, {Name: "name", Type: "text"}},
						Rows:    [][]any{{big, "ada"}}},
				},
				Root: Root{Node: "r", Cardinality: "many"},
			}),
			Read("mine", Query{
				Nodes: map[string]Node{
					"u": {Kind: "scan", Table: "users", Scope: "u"},
					"o": {Kind: "order", Input: "u", Terms: []OrderTerm{{Expr: *Col("u", "id")}}},
				},
				Root: Root{Node: "o", Cardinality: "many"},
			}),
		},
		Result: "mine",
	}

	raw, err := MarshalProgram(p)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	got, err := UnmarshalProgram(raw)
	if err != nil {
		t.Fatalf("unmarshal: %v\n%s", err, raw)
	}
	if !reflect.DeepEqual(got, p) {
		t.Fatalf("program drifted over the wire.\n got: %#v\nwant: %#v", got, p)
	}

	// The int64 beyond float53 survived inside the nested LIR relation.
	cell := got.Statements[0].Relation.Nodes["r"].Rows[0][0]
	n, ok := cell.(json.Number)
	if !ok || n.String() != "9007199254740993" {
		t.Fatalf("int64 precision lost through the statement relation: %T %v", cell, cell)
	}
}

// A single-statement program needs no result selector, and QueryProgram builds
// exactly that.
func TestQueryProgramSingleStatement(t *testing.T) {
	raw, err := MarshalProgram(QueryProgram(oneRow("n", 1)))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if _, err := UnmarshalProgram(raw); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
}

func TestProgramValidationRejections(t *testing.T) {
	cases := []struct {
		name    string
		program Program
		detail  string
	}{
		{
			name:    "empty program",
			program: Program{},
			detail:  "minItems",
		},
		{
			name: "duplicate statement names",
			program: Program{Statements: []Statement{
				Read("dup", oneRow("n", 1)),
				Read("dup", oneRow("n", 2)),
			}, Result: "dup"},
			detail: "duplicate statement name",
		},
		{
			name: "multi-statement without result",
			program: Program{Statements: []Statement{
				Read("a", oneRow("n", 1)),
				Read("b", oneRow("n", 2)),
			}},
			detail: "must name its result",
		},
		{
			name: "result names unknown statement",
			program: Program{Statements: []Statement{
				Read("a", oneRow("n", 1)),
			}, Result: "ghost"},
			detail: "unknown statement",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := MarshalProgram(tc.program)
			if err == nil {
				t.Fatalf("program should be rejected")
			}
			if !strings.Contains(err.Error(), tc.detail) {
				t.Fatalf("error %q should mention %q", err, tc.detail)
			}
		})
	}
}

// A statement carrying a malformed LIR relation is rejected by the two-phase
// pass, and the error names the offending statement.
func TestProgramRejectsMalformedRelation(t *testing.T) {
	// A scan with no table is a valid PIR envelope but invalid LIR.
	bad := Program{Statements: []Statement{
		Read("broken", Query{
			Nodes: map[string]Node{"s": {Kind: "scan", Scope: "s"}},
			Root:  Root{Node: "s", Cardinality: "many"},
		}),
	}}
	raw, err := MarshalProgram(bad)
	if err == nil {
		t.Fatalf("malformed relation should be rejected, got %s", raw)
	}
	if !strings.Contains(err.Error(), "broken") {
		t.Fatalf("error should name the statement, got %q", err)
	}
}

// The statement kind discriminates the union: a wire document with an unknown
// kind is rejected with a best-match message naming the statement.
func TestProgramRejectsUnknownKind(t *testing.T) {
	raw := []byte(`{"statements":[{"kind":"upsert","name":"x","table":"t","relation":{}}]}`)
	err := ValidatePIRJSON(raw)
	if err == nil {
		t.Fatal("unknown statement kind should be rejected")
	}
	if !strings.Contains(err.Error(), "upsert") {
		t.Fatalf("error should name the unknown kind, got %q", err)
	}
}
