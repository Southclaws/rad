package protocol

// The query graph survives the full wire codec: plain types → generated
// types → JSON → back, with node maps, recursive expressions, crossings,
// value-less operators, and full int64 precision intact.

import (
	"encoding/json"
	"reflect"
	"testing"

	"github.com/go-faster/jx"

	"rad/protocol/oas"
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
			"out": {Kind: "project", Input: "boards", Spread: []string{"b"}, Fields: []Field{
				{As: "tasks", Expr: *ArrayOf("page")},
				{As: "stats", Expr: *FirstOf("stats")},
				{As: "busy", Expr: *Exists("open")},
			}},
		},
		Root: Root{Node: "out", Cardinality: "many"},
	}

	// Through the generated codec and raw JSON bytes, exactly as the wire
	// carries it.
	enc := jx.GetEncoder()
	o := QueryToOAS(q)
	o.Encode(enc)
	raw := enc.Bytes()

	var back oas.Query
	dec := jx.DecodeBytes(raw)
	if err := back.Decode(dec); err != nil {
		t.Fatalf("decode: %v\n%s", err, raw)
	}
	got := QueryFromOAS(back)

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
