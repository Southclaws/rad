package server

import (
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

func TestGraphQueryRequiresExactRelationTree(t *testing.T) {
	t.Run("valid", func(t *testing.T) {
		q := protocol.Query{
			Nodes: map[string]protocol.Node{
				"boards": {Kind: "scan", Table: "boards", Scope: "b"},
				"tasks":  {Kind: "scan", Table: "tasks", Scope: "t"},
				"mine": {Kind: "filter", Input: "tasks", Predicate: protocol.Eq(
					protocol.Col("t", "board_id"), protocol.Col("b", "id"),
				)},
				"out": {Kind: "project", Input: "boards", Spread: []string{"b"}, Fields: []protocol.Field{
					{As: "tasks", Expr: *protocol.ArrayOf("mine")},
				}},
			},
			Root: protocol.Root{Node: "out", Cardinality: "many"},
		}
		if _, err := graphQuery(q); err != nil {
			t.Fatal(err)
		}
	})

	t.Run("dangling", func(t *testing.T) {
		q := graphScanQuery()
		q.Nodes["out"] = protocol.Node{Kind: "filter", Input: "ghost", Predicate: protocol.Lit(true)}
		q.Root.Node = "out"
		graphErr(t, q, `unknown node "ghost"`)
	})

	t.Run("direct cycle", func(t *testing.T) {
		q := graphScanQuery()
		q.Nodes["loop"] = protocol.Node{Kind: "filter", Input: "loop", Predicate: protocol.Lit(true)}
		q.Root.Node = "loop"
		delete(q.Nodes, "scan")
		graphErr(t, q, "part of a cycle")
	})

	t.Run("indirect cycle", func(t *testing.T) {
		q := graphScanQuery()
		q.Nodes = map[string]protocol.Node{
			"a": {Kind: "filter", Input: "b", Predicate: protocol.Lit(true)},
			"b": {Kind: "filter", Input: "a", Predicate: protocol.Lit(true)},
		}
		q.Root.Node = "a"
		graphErr(t, q, "part of a cycle")
	})

	t.Run("shared input", func(t *testing.T) {
		q := graphScanQuery()
		q.Nodes["left"] = protocol.Node{Kind: "filter", Input: "scan", Predicate: protocol.Lit(true)}
		q.Nodes["right"] = protocol.Node{Kind: "filter", Input: "scan", Predicate: protocol.Lit(true)}
		q.Nodes["out"] = protocol.Node{Kind: "join", Left: "left", Right: "right", Join: "inner", On: protocol.Lit(true)}
		q.Root.Node = "out"
		graphErr(t, q, "multiple consumers")
	})

	t.Run("repeated crossing", func(t *testing.T) {
		q := graphScanQuery()
		q.Nodes["child"] = protocol.Node{Kind: "scan", Table: "tasks", Scope: "t"}
		q.Nodes["out"] = protocol.Node{Kind: "project", Input: "scan", Fields: []protocol.Field{
			{As: "a", Expr: *protocol.Exists("child")},
			{As: "b", Expr: *protocol.Exists("child")},
		}}
		q.Root.Node = "out"
		graphErr(t, q, "multiple consumers")
	})

	t.Run("unreachable", func(t *testing.T) {
		q := graphScanQuery()
		q.Nodes["unused"] = protocol.Node{Kind: "scan", Table: "tasks", Scope: "unused"}
		graphErr(t, q, "unreachable node definitions: [unused]")
	})
}

func TestGraphQueryRejectsMalformedRecursiveExpressions(t *testing.T) {
	q := graphScanQuery()
	q.Nodes["bad"] = protocol.Node{
		Kind:  "filter",
		Input: "scan",
		Predicate: &protocol.Expr{
			Kind: "binary", Op: "eq", Left: protocol.Col("s", "id"),
		},
	}
	q.Root.Node = "bad"
	graphErr(t, q, "right operand needs an expression")
}

func TestGraphQueryRejectsStructurallyEmptyOperators(t *testing.T) {
	q := graphScanQuery()
	q.Nodes["empty"] = protocol.Node{Kind: "project", Input: "scan"}
	q.Root.Node = "empty"
	graphErr(t, q, "needs at least one spread scope or field")

	q = graphScanQuery()
	q.Nodes["empty"] = protocol.Node{Kind: "aggregate", Input: "scan"}
	q.Root.Node = "empty"
	graphErr(t, q, "needs at least one group or aggregate")

	q = graphScanQuery()
	q.Nodes["empty"] = protocol.Node{Kind: "order", Input: "scan"}
	q.Root.Node = "empty"
	graphErr(t, q, "needs at least one term")
}

func graphScanQuery() protocol.Query {
	return protocol.Query{
		Nodes: map[string]protocol.Node{"scan": {Kind: "scan", Table: "tasks", Scope: "s"}},
		Root:  protocol.Root{Node: "scan", Cardinality: "many"},
	}
}

func graphErr(t *testing.T, q protocol.Query, want string) {
	t.Helper()
	_, err := graphQuery(q)
	if err == nil || !strings.Contains(err.Error(), want) {
		t.Fatalf("error = %v, want substring %q", err, want)
	}
}
