package planner

import (
	"strconv"

	"github.com/Southclaws/rad/rad/protocol"
)

// q builds an observable test query. Collection fixtures choose their
// semantic order here, by the first scalar output of a projection or
// aggregate, and by id for a base relation. Tests that exercise an invalid
// ordering shape construct protocol.Query directly instead.
func q(nodes map[string]protocol.Node, root, cardinality string) protocol.Query {
	for name, node := range nodes {
		walkNode(nodes, name, &node)
		nodes[name] = node
	}
	root = observableRoot(nodes, root, cardinality)
	return protocol.Query{Nodes: nodes, Root: protocol.Root{Node: root, Cardinality: cardinality}}
}

func observableRoot(nodes map[string]protocol.Node, root, cardinality string) string {
	if cardinality == "many" {
		return ordered(nodes, root)
	}
	return root
}

func walkNode(nodes map[string]protocol.Node, name string, node *protocol.Node) {
	for i := range node.Fields {
		walkExpr(nodes, &node.Fields[i].Expr)
	}
	for i := range node.Groups {
		walkExpr(nodes, &node.Groups[i].Expr)
	}
	for i := range node.Aggs {
		walkExpr(nodes, node.Aggs[i].Arg)
	}
	for i := range node.Terms {
		walkExpr(nodes, &node.Terms[i].Expr)
	}
	walkExpr(nodes, node.On)
	walkExpr(nodes, node.Predicate)
}

func walkExpr(nodes map[string]protocol.Node, expr *protocol.Expr) {
	if expr == nil {
		return
	}
	if expr.Kind == "array" || expr.Kind == "first" {
		expr.Node = ordered(nodes, expr.Node)
	}
	walkExpr(nodes, expr.Expr)
	walkExpr(nodes, expr.Left)
	walkExpr(nodes, expr.Right)
}

func ordered(nodes map[string]protocol.Node, root string) string {
	node, exists := nodes[root]
	if !exists || orderedPath(nodes, root) {
		return root
	}
	if node.Kind == "slice" {
		node.Input = ordered(nodes, node.Input)
		nodes[root] = node
		return root
	}

	scope, column := orderKey(nodes, root, node)
	name := root + "_ordered"
	for i := 2; ; i++ {
		if _, exists := nodes[name]; !exists {
			break
		}
		name = root + "_ordered_" + strconv.Itoa(i)
	}
	nodes[name] = protocol.Node{
		Kind:  "order",
		Input: root,
		Terms: []protocol.OrderTerm{{Expr: *protocol.Col(scope, column)}},
	}
	return name
}

func orderedPath(nodes map[string]protocol.Node, root string) bool {
	node, exists := nodes[root]
	if !exists {
		return false
	}
	switch node.Kind {
	case "order":
		return true
	case "filter", "project", "slice":
		return orderedPath(nodes, node.Input)
	}
	return false
}

func orderKey(nodes map[string]protocol.Node, root string, node protocol.Node) (string, string) {
	switch node.Kind {
	case "project":
		if node.Scope == "" {
			node.Scope = root + "_result"
			nodes[root] = node
		}
		if len(node.Spread) > 0 {
			return node.Scope, "id"
		}
		for _, field := range node.Fields {
			if field.Expr.Kind != "array" && field.Expr.Kind != "first" {
				return node.Scope, field.As
			}
		}
		return node.Scope, "id"
	case "aggregate":
		if node.Scope == "" {
			node.Scope = root + "_result"
			nodes[root] = node
		}
		if len(node.Groups) > 0 {
			name := node.Groups[0].As
			if name == "" {
				name = node.Groups[0].Expr.Column
			}
			return node.Scope, name
		}
		return node.Scope, node.Aggs[0].As
	case "scan", "ref":
		return node.Scope, "id"
	case "filter", "join":
		return sourceScope(nodes, root), "id"
	}
	return sourceScope(nodes, root), "id"
}

func sourceScope(nodes map[string]protocol.Node, root string) string {
	node := nodes[root]
	switch node.Kind {
	case "scan", "ref", "project", "aggregate":
		return node.Scope
	case "filter", "order", "slice":
		return sourceScope(nodes, node.Input)
	case "join":
		return sourceScope(nodes, node.Left)
	}
	return ""
}
