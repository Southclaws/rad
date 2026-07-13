package planner

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

func TestObservableCollectionsRequireOrder(t *testing.T) {
	t.Parallel()
	d := tracker(t)

	d.Query(protocol.Query{
		Nodes: map[string]protocol.Node{
			"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		},
		Root: protocol.Root{Node: "t", Cardinality: "many"},
	}).ExpectStatus(422).ExpectCode("invalid").ExpectError(`root cardinality "many" needs an explicit order`)

	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"by_id": {Kind: "order", Input: "t",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("t", "id")}}},
	}, "by_id", "many")).Len(4)

	d.Query(protocol.Query{Nodes: map[string]protocol.Node{
		"u": {Kind: "scan", Table: "users", Scope: "u"},
		"ada": {Kind: "filter", Input: "u",
			Predicate: protocol.Eq(protocol.Col("u", "name"), protocol.Lit("Ada"))},
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"out": {Kind: "project", Input: "ada", Fields: []protocol.Field{
			{As: "tasks", Expr: *protocol.ArrayOf("t")},
		}},
	}, Root: protocol.Root{Node: "out", Cardinality: "first"}}).ExpectStatus(422).ExpectCode("invalid").ExpectError("array over an unordered relation")

	d.Query(protocol.Query{Nodes: map[string]protocol.Node{
		"t":    {Kind: "scan", Table: "tasks", Scope: "t"},
		"page": {Kind: "slice", Input: "t", Offset: new(int(1))},
	}, Root: protocol.Root{Node: "page", Cardinality: "first"}}).ExpectStatus(422).ExpectCode("invalid").ExpectError("slice offset over an unordered relation")

	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"by_id": {Kind: "order", Input: "t",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("t", "id")}}},
		"page": {Kind: "slice", Input: "by_id", Offset: new(int(1)), Limit: new(int(2))},
	}, "page", "many")).Equals(`[
		{"id":"t2","board_id":"b1","title":"write","status":"open","priority":5,"estimate":null,"assignee_id":null},
		{"id":"t3","board_id":"b1","title":"done","status":"done","priority":9,"estimate":null,"assignee_id":"ada"}
	]`)

	d.Query(q(map[string]protocol.Node{
		"t":   {Kind: "scan", Table: "tasks", Scope: "t"},
		"one": {Kind: "slice", Input: "t", Limit: new(int(1))},
	}, "one", "first")).Len(1)
}
