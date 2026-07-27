package planner

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

func TestObservableCollectionsRequireOrder(t *testing.T) {
	t.Parallel()
	d := tracker(t)

	d.Query(lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"t": lirwire.Scan("tasks", "t"),
		},
		Root: lirwire.Root{Node: "t", Cardinality: "many"},
	}).ExpectStatus(422).ExpectCode("invalid").ExpectError(`root cardinality "many" needs an explicit order`)

	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"by_id": lirwire.Order("t",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("t", "id")}}),
	}, "by_id", "many")).Len(4)

	d.Query(lirwire.Query{Nodes: map[string]lirwire.Node{
		"u": lirwire.Scan("users", "u"),
		"ada": lirwire.Filter("u",
			lirwire.Binary("eq", lirwire.Col("u", "name"), lirwire.LitOf("Ada"))),
		"t": lirwire.Scan("tasks", "t"),
		"out": lirwire.Project("ada", "", nil, []lirwire.Field{
			{As: "tasks", Expr: lirwire.Array("t")},
		}),
	}, Root: lirwire.Root{Node: "out", Cardinality: "first"}}).ExpectStatus(422).ExpectCode("invalid").ExpectError("array over an unordered relation")

	d.Query(lirwire.Query{Nodes: map[string]lirwire.Node{
		"t":    lirwire.Scan("tasks", "t"),
		"page": lirwire.Slice("t", 1, nil),
	}, Root: lirwire.Root{Node: "page", Cardinality: "first"}}).ExpectStatus(422).ExpectCode("invalid").ExpectError("slice offset over an unordered relation")

	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"by_id": lirwire.Order("t",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("t", "id")}}),
		"page": lirwire.Slice("by_id", 1, ptrInt(2)),
	}, "page", "many")).Equals(`[
		{"id":"t2","board_id":"b1","title":"write","status":"open","priority":5,"estimate":null,"assignee_id":null},
		{"id":"t3","board_id":"b1","title":"done","status":"done","priority":9,"estimate":null,"assignee_id":"ada"}
	]`)

	d.Query(q(map[string]lirwire.Node{
		"t":   lirwire.Scan("tasks", "t"),
		"one": lirwire.Slice("t", 0, ptrInt(1)),
	}, "one", "first")).Len(1)
}
