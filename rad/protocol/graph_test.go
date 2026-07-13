package protocol

// The query tree survives the Schemancer-generated wire codec with node maps,
// recursive expressions, crossings, value-less operators, and full int64
// precision intact.

import (
	"encoding/json"
	"reflect"
	"testing"
)

func TestGraphRoundTrip(t *testing.T) {
	big := json.Number("9007199254740993") // > 2^53
	q := Query{
		Nodes: map[string]Node{
			"boards": {Kind: "scan", Table: "boards", Scope: "b"},
			"tasks":  {Kind: "scan", Table: "tasks", Scope: "t"},
			"open": {Kind: "filter", Input: "tasks", Predicate: AndAll([]*Expr{
				Eq(Col("t", "board_id"), Col("b", "id")),
				Eq(Col("t", "status"), Lit("open")),
				Gte(Col("t", "priority"), Lit(big)),
				Not(IsNull(Col("t", "assignee_id"))),
			})},
			"sorted": {Kind: "order", Input: "open", Terms: []OrderTerm{
				{Expr: *Col("t", "priority"), Desc: true},
			}},
			"page": {Kind: "slice", Input: "sorted", Limit: new(int(20))},
			"stats": {Kind: "aggregate", Input: "open",
				Groups: []GroupTerm{{Expr: *Col("t", "status")}},
				Aggs: []AggTerm{
					{Fn: "count", As: "n"},
					{Fn: "avg", Arg: Col("t", "estimate"), As: "mean"},
				}},
			"joined": {Kind: "join", Left: "boards", Right: "tasks", Join: "left",
				On: Eq(Col("b", "id"), Col("t", "board_id"))},
			"out": {Kind: "project", Input: "boards", Spread: []string{"b"}, Fields: []Field{
				{As: "tasks", Expr: *ArrayOf("page")},
				{As: "stats", Expr: *FirstOf("stats")},
				{As: "busy", Expr: *Exists("open")},
				{As: "count", Expr: *ScalarOf("stats")},
				{As: "cast", Expr: Expr{Kind: "cast", Expr: Lit(json.Number("1")), To: "float64"}},
			}},
		},
		Root: Root{Node: "out", Cardinality: "many"},
	}

	raw, err := MarshalQuery(q)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	got, err := UnmarshalQuery(raw)
	if err != nil {
		t.Fatalf("unmarshal: %v\n%s", err, raw)
	}

	if !reflect.DeepEqual(got, q) {
		t.Fatalf("graph drifted over the wire.\n got: %#v\nwant: %#v", got, q)
	}

	// The integer beyond float53 survives as a json.Number.
	pred := got.Nodes["open"].Predicate
	gte := pred.Left.Right // and(and(and(eq,eq),gte),not) → left.right = gte
	if gte.Op != "gte" {
		t.Fatalf("fold shape changed: %+v", pred)
	}
	n, ok := gte.Right.Value.(json.Number)
	if !ok || n.String() != "9007199254740993" {
		t.Fatalf("int64 precision lost: %T %v", gte.Right.Value, gte.Right.Value)
	}
}

func TestStrictQueryWireValidation(t *testing.T) {
	cases := []struct {
		name string
		raw  string
	}{
		{"missing nodes", `{"root":{"node":"s","cardinality":"many"}}`},
		{"premature version field", `{"lir_version":1,"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"}},"root":{"node":"s","cardinality":"many"}}`},
		{"unknown query field", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"}},"root":{"node":"s","cardinality":"many"},"extra":true}`},
		{"wrong kind field", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s","predicate":{"kind":"lit","value":true}}},"root":{"node":"s","cardinality":"many"}}`},
		{"missing predicate", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s"}},"root":{"node":"f","cardinality":"many"}}`},
		{"missing binary operand", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"binary","op":"eq","left":{"kind":"lit","value":1}}}},"root":{"node":"f","cardinality":"many"}}`},
		{"unknown operator", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"binary","op":"wat","left":{"kind":"lit","value":1},"right":{"kind":"lit","value":1}}}},"root":{"node":"f","cardinality":"many"}}`},
		{"removed call expression", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"f":{"kind":"filter","input":"s","predicate":{"kind":"call","fn":"lower","args":[{"kind":"lit","value":"x"}]}}},"root":{"node":"f","cardinality":"many"}}`},
		{"negative slice", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"p":{"kind":"slice","input":"s","limit":-1}},"root":{"node":"p","cardinality":"many"}}`},
		{"empty projection", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"p":{"kind":"project","input":"s"}},"root":{"node":"p","cardinality":"many"}}`},
		{"empty aggregate", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"a":{"kind":"aggregate","input":"s"}},"root":{"node":"a","cardinality":"many"}}`},
		{"empty order", `{"nodes":{"s":{"kind":"scan","table":"tasks","scope":"s"},"o":{"kind":"order","input":"s","terms":[]}},"root":{"node":"o","cardinality":"many"}}`},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			if err := ValidateLIRJSON([]byte(tc.raw)); err == nil {
				t.Fatalf("invalid query accepted: %s", tc.raw)
			}
		})
	}
}
