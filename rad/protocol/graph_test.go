package protocol

// The query tree survives the Schemancer-generated wire codec with node maps,
// recursive expressions, crossings, value-less operators, and full int64
// precision intact.

import (
	"encoding/json"
	"reflect"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

func TestGraphRoundTrip(t *testing.T) {
	big := json.Number("9007199254740993") // > 2^53
	q := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"boards": lirwire.Scan("boards", "b"),
			"tasks":  lirwire.Scan("tasks", "t"),
			"open": lirwire.Filter("tasks", lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("t", "board_id"), lirwire.Col("b", "id")),
				lirwire.Binary("eq", lirwire.Col("t", "status"), lirwire.LitOf("open")),
				lirwire.Binary("gte", lirwire.Col("t", "priority"), lirwire.LitOf(big)),
				lirwire.Unary("not", lirwire.Unary("is_null", lirwire.Col("t", "assignee_id"))),
			})),
			"sorted": lirwire.Order("open", []lirwire.OrderTerm{
				{Expr: lirwire.Col("t", "priority"), Desc: ptrBool(true)},
			}),
			"page": lirwire.Slice("sorted", 0, ptrInt(20)),
			"stats": lirwire.Aggregate("open", "",
				[]lirwire.GroupTerm{{Expr: lirwire.Col("t", "status")}},
				[]lirwire.AggTerm{
					{Fn: "count", As: "n"},
					{Fn: "avg", Arg: ptrExpr(lirwire.Col("t", "estimate")), As: "mean"},
				}),
			"joined": lirwire.Join("boards", "tasks", "left",
				lirwire.Binary("eq", lirwire.Col("b", "id"), lirwire.Col("t", "board_id"))),
			"out": lirwire.Project("boards", "", []string{"b"}, []lirwire.Field{
				{As: "tasks", Expr: lirwire.Array("page")},
				{As: "stats", Expr: lirwire.First("stats")},
				{As: "busy", Expr: lirwire.Exists("open")},
				{As: "count", Expr: lirwire.Scalar("stats")},
				{As: "cast", Expr: lirwire.Cast(lirwire.LitOf(json.Number("1")), "float64")},
			}),
		},
		Root: lirwire.Root{Node: "out", Cardinality: "many"},
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

	// The integer beyond float53 survives as its exact raw JSON bytes. Navigate
	// the AndAll fold: and(and(and(eq,eq),gte),not), so the gte is top.Left.Right.
	fn, ok := got.Nodes["open"].NodeUnion.(*lirwire.FilterNode)
	if !ok {
		t.Fatalf("open node is not a filter: %T", got.Nodes["open"].NodeUnion)
	}
	top, ok := fn.Predicate.ExprUnion.(*lirwire.BinaryExpr)
	if !ok {
		t.Fatalf("predicate is not binary: %T", fn.Predicate.ExprUnion)
	}
	mid, ok := top.Left.ExprUnion.(*lirwire.BinaryExpr)
	if !ok {
		t.Fatalf("fold shape changed: %#v", top)
	}
	gte, ok := mid.Right.ExprUnion.(*lirwire.BinaryExpr)
	if !ok || gte.Op != "gte" {
		t.Fatalf("fold shape changed: %#v", mid)
	}
	lit, ok := gte.Right.ExprUnion.(*lirwire.LiteralExpr)
	if !ok {
		t.Fatalf("gte right is not a literal: %T", gte.Right.ExprUnion)
	}
	if string(lit.Value) != "9007199254740993" {
		t.Fatalf("int64 precision lost: %s", lit.Value)
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
