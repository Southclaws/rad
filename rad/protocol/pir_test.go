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

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// oneRow is a one-row constant relation, the simplest valid LIR document.
func oneRow(col string, v any) lirwire.Query {
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"r": lirwire.Rows("r",
				[]lirwire.RowsColumn{{Name: col, Type: "int64"}},
				[][]lirwire.Cell{{mustValue(v)}}),
		},
		Root: lirwire.Root{Node: "r", Cardinality: "many"},
	}
}

func TestProgramRoundTrip(t *testing.T) {
	big := json.Number("9007199254740993") // > 2^53
	authorRel := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"r": lirwire.Rows("r",
				[]lirwire.RowsColumn{{Name: "id", Type: "int64"}, {Name: "name", Type: "text"}},
				[][]lirwire.Cell{{mustValue(big), mustValue("ada")}}),
		},
		Root: lirwire.Root{Node: "r", Cardinality: "many"},
	}
	mineRel := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"u": lirwire.Scan("users", "u"),
			"o": lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "id")}}),
		},
		Root: lirwire.Root{Node: "o", Cardinality: "many"},
	}
	p := pirwire.Prog("mine",
		pirwire.Create("author", "users", relBytes(authorRel)),
		pirwire.Query("mine", relBytes(mineRel)),
	)

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

	// The int64 beyond float53 survived inside the nested LIR relation. The
	// statement relation is opaque bytes; decode it back into an LIR query and
	// assert the cell's raw JSON bytes are exactly the big integer.
	create, ok := got.Statements[0].StatementUnion.(*pirwire.CreateStatement)
	if !ok {
		t.Fatalf("first statement is not a create: %T", got.Statements[0].StatementUnion)
	}
	var rel lirwire.Query
	if err := json.Unmarshal(create.Relation, &rel); err != nil {
		t.Fatalf("decode statement relation: %v", err)
	}
	rows, ok := rel.Nodes["r"].NodeUnion.(*lirwire.RowsNode)
	if !ok {
		t.Fatalf("relation node r is not rows: %T", rel.Nodes["r"].NodeUnion)
	}
	if cell := rows.Rows[0][0]; cell == nil || *cell != "9007199254740993" {
		t.Fatalf("int64 precision lost through the statement relation: %v", cell)
	}
}

// A single-statement program needs no result selector, and QueryProgram builds
// exactly that.
func TestQueryProgramSingleStatement(t *testing.T) {
	prog := pirwire.Prog("", pirwire.Query("result", relBytes(oneRow("n", 1))))
	raw, err := MarshalProgram(prog)
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
		program pirwire.Program
		detail  string
	}{
		{
			name:    "empty program",
			program: pirwire.Program{Statements: []pirwire.Statement{}},
			detail:  "minItems",
		},
		{
			name: "duplicate statement names",
			program: pirwire.Prog("dup",
				pirwire.Query("dup", relBytes(oneRow("n", 1))),
				pirwire.Query("dup", relBytes(oneRow("n", 2))),
			),
			detail: "duplicate statement name",
		},
		{
			name: "multi-statement without result",
			program: pirwire.Prog("",
				pirwire.Query("a", relBytes(oneRow("n", 1))),
				pirwire.Query("b", relBytes(oneRow("n", 2))),
			),
			detail: "must name its result",
		},
		{
			name: "result names unknown statement",
			program: pirwire.Prog("ghost",
				pirwire.Query("a", relBytes(oneRow("n", 1))),
			),
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
	bad := pirwire.Prog("", pirwire.Query("broken", relBytes(lirwire.Query{
		Nodes: map[string]lirwire.Node{"s": lirwire.Scan("", "s")},
		Root:  lirwire.Root{Node: "s", Cardinality: "many"},
	})))
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
