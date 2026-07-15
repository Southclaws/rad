package planner

import (
	"encoding/json"
	"strconv"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// -
// shared test helpers
// -

// relBytes marshals a wire query into a statement's opaque relation payload.
func relBytes(q lirwire.Query) []byte { b, _ := json.Marshal(q); return b }

// mustValue formats a Go scalar as a raw wire Value, ignoring the (impossible
// for JSON-encodable scalars) error.
func mustValue(v any) lirwire.Value { val, _ := lirwire.SetAny(v); return val }

func ptrBool(b bool) *bool                 { return &b }
func ptrStr(s string) *string              { return &s }
func ptrInt(n int) *int                    { return &n }
func ptrExpr(e lirwire.Expr) *lirwire.Expr { return &e }

// q builds an observable test query. Collection fixtures choose their
// semantic order here, by the first scalar output of a projection or
// aggregate, and by id for a base relation. Tests that exercise an invalid
// ordering shape construct lirwire.Query directly instead.
func q(nodes map[string]lirwire.Node, root, cardinality string) lirwire.Query {
	for name, node := range nodes {
		walkNode(nodes, name, &node)
		nodes[name] = node
	}
	root = observableRoot(nodes, root, cardinality)
	return lirwire.Query{Nodes: nodes, Root: lirwire.Root{Node: root, Cardinality: cardinality}}
}

func observableRoot(nodes map[string]lirwire.Node, root, cardinality string) string {
	if cardinality == "many" {
		return ordered(nodes, root)
	}
	return root
}

func walkNode(nodes map[string]lirwire.Node, name string, node *lirwire.Node) {
	switch n := node.NodeUnion.(type) {
	case *lirwire.ProjectNode:
		for i := range n.Fields {
			walkExpr(nodes, &n.Fields[i].Expr)
		}
	case *lirwire.AggregateNode:
		for i := range n.Groups {
			walkExpr(nodes, &n.Groups[i].Expr)
		}
		for i := range n.Aggs {
			walkExpr(nodes, n.Aggs[i].Arg)
		}
	case *lirwire.OrderNode:
		for i := range n.Terms {
			walkExpr(nodes, &n.Terms[i].Expr)
		}
	case *lirwire.JoinNode:
		walkExpr(nodes, &n.On)
	case *lirwire.FilterNode:
		walkExpr(nodes, &n.Predicate)
	}
}

func walkExpr(nodes map[string]lirwire.Node, expr *lirwire.Expr) {
	if expr == nil || expr.ExprUnion == nil {
		return
	}
	switch e := expr.ExprUnion.(type) {
	case *lirwire.CrossingExprArray:
		e.Node = ordered(nodes, e.Node)
	case *lirwire.CrossingExprFirst:
		e.Node = ordered(nodes, e.Node)
	case *lirwire.UnaryExpr:
		walkExpr(nodes, &e.Expr)
	case *lirwire.BinaryExpr:
		walkExpr(nodes, &e.Left)
		walkExpr(nodes, &e.Right)
	case *lirwire.CastExpr:
		walkExpr(nodes, &e.Expr)
	}
}

func ordered(nodes map[string]lirwire.Node, root string) string {
	node, exists := nodes[root]
	if !exists || orderedPath(nodes, root) {
		return root
	}
	if s, ok := node.NodeUnion.(*lirwire.SliceNode); ok {
		s.Input = ordered(nodes, s.Input)
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
	nodes[name] = lirwire.Order(root, []lirwire.OrderTerm{{Expr: lirwire.Col(scope, column)}})
	return name
}

func orderedPath(nodes map[string]lirwire.Node, root string) bool {
	node, exists := nodes[root]
	if !exists {
		return false
	}
	switch n := node.NodeUnion.(type) {
	case *lirwire.OrderNode:
		return true
	case *lirwire.FilterNode:
		return orderedPath(nodes, n.Input)
	case *lirwire.ProjectNode:
		return orderedPath(nodes, n.Input)
	case *lirwire.SliceNode:
		return orderedPath(nodes, n.Input)
	}
	return false
}

func orderKey(nodes map[string]lirwire.Node, root string, node lirwire.Node) (string, string) {
	switch n := node.NodeUnion.(type) {
	case *lirwire.ProjectNode:
		scope := derefStr(n.Scope)
		if scope == "" {
			scope = root + "_result"
			n.Scope = &scope
		}
		if len(n.Spread) > 0 {
			return scope, "id"
		}
		for _, field := range n.Fields {
			switch field.Expr.ExprUnion.(type) {
			case *lirwire.CrossingExprArray, *lirwire.CrossingExprFirst:
				// a nested value is not a natural scalar order key
			default:
				return scope, field.As
			}
		}
		return scope, "id"
	case *lirwire.AggregateNode:
		scope := derefStr(n.Scope)
		if scope == "" {
			scope = root + "_result"
			n.Scope = &scope
		}
		if len(n.Groups) > 0 {
			name := derefStr(n.Groups[0].As)
			if name == "" {
				if col, ok := n.Groups[0].Expr.ExprUnion.(*lirwire.ColumnExpr); ok {
					name = col.Column
				}
			}
			return scope, name
		}
		return scope, n.Aggs[0].As
	case *lirwire.ScanNode:
		return n.Scope, "id"
	case *lirwire.RefNode:
		return n.Scope, "id"
	case *lirwire.RowsNode:
		return n.Scope, n.Columns[0].Name
	case *lirwire.FilterNode:
		return sourceKey(nodes, root)
	case *lirwire.JoinNode:
		return sourceKey(nodes, root)
	}
	return sourceKey(nodes, root)
}

// sourceKey walks to the underlying source and picks its natural order key:
// id for tables and refs, the first declared column for a constant relation.
func sourceKey(nodes map[string]lirwire.Node, root string) (string, string) {
	switch n := nodes[root].NodeUnion.(type) {
	case *lirwire.ScanNode:
		return n.Scope, "id"
	case *lirwire.RefNode:
		return n.Scope, "id"
	case *lirwire.ProjectNode:
		return derefStr(n.Scope), "id"
	case *lirwire.AggregateNode:
		return derefStr(n.Scope), "id"
	case *lirwire.RowsNode:
		return n.Scope, n.Columns[0].Name
	case *lirwire.FilterNode:
		return sourceKey(nodes, n.Input)
	case *lirwire.OrderNode:
		return sourceKey(nodes, n.Input)
	case *lirwire.SliceNode:
		return sourceKey(nodes, n.Input)
	case *lirwire.JoinNode:
		return sourceKey(nodes, n.Left)
	}
	return "", "id"
}

func derefStr(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}
