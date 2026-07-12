package lirwire

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// One raw JSON scalar: string, number, boolean, or null. The binder preserves JSON numbers and types literals from context.
type Value = json.RawMessage

// A strict, single-consumer relation tree encoded as named inline node definitions plus a root selector. The normative semantics live in docs/design/lir.md.
type ExprUnion interface {
	ExprType() string
	isExpr()
}

type Expr struct {
	ExprUnion
}

func (w Expr) MarshalJSON() ([]byte, error) {
	if w.ExprUnion == nil {
		return []byte("null"), nil
	}
	return json.Marshal(w.ExprUnion)
}

func (w *Expr) UnmarshalJSON(data []byte) error {
	data = bytes.TrimSpace(data)
	if bytes.Equal(data, []byte("null")) {
		w.ExprUnion = nil
		return nil
	}

	var peek struct {
		Type string `json:"kind"`
	}
	if err := json.Unmarshal(data, &peek); err != nil {
		return fmt.Errorf("Expr: invalid JSON: %w", err)
	}
	if peek.Type == "" {
		return fmt.Errorf("Expr: missing discriminator field %q", "kind")
	}

	var v ExprUnion
	switch peek.Type {
	case "lit":
		v = &LiteralExpr{}
	case "col":
		v = &ColumnExpr{}
	case "unary":
		v = &UnaryExpr{}
	case "binary":
		v = &BinaryExpr{}
	case "call":
		v = &CallExpr{}
	case "cast":
		v = &CastExpr{}
	case "exists":
		v = &CrossingExprExists{}
	case "first":
		v = &CrossingExprFirst{}
	case "scalar":
		v = &CrossingExprScalar{}
	case "array":
		v = &CrossingExprArray{}
	default:
		return fmt.Errorf("Expr: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("Expr: invalid %q payload: %w", peek.Type, err)
	}

	w.ExprUnion = v
	return nil
}

type LiteralExpr struct {
	Kind  string `json:"kind"`
	Value Value  `json:"value"`
}

func (LiteralExpr) isExpr() {}

func (LiteralExpr) ExprType() string { return "lit" }

type ColumnExpr struct {
	Column string `json:"column"`
	Kind   string `json:"kind"`
	Scope  string `json:"scope"`
}

func (ColumnExpr) isExpr() {}

func (ColumnExpr) ExprType() string { return "col" }

type UnaryExpr struct {
	Expr Expr   `json:"expr"`
	Kind string `json:"kind"`
	Op   string `json:"op"`
}

func (UnaryExpr) isExpr() {}

func (UnaryExpr) ExprType() string { return "unary" }

type BinaryExpr struct {
	Kind  string `json:"kind"`
	Left  Expr   `json:"left"`
	Op    string `json:"op"`
	Right Expr   `json:"right"`
}

func (BinaryExpr) isExpr() {}

func (BinaryExpr) ExprType() string { return "binary" }

type CallExpr struct {
	Args []Expr `json:"args"`
	Fn   string `json:"fn"`
	Kind string `json:"kind"`
}

func (CallExpr) isExpr() {}

func (CallExpr) ExprType() string { return "call" }

type CastExpr struct {
	Expr Expr   `json:"expr"`
	Kind string `json:"kind"`
	To   string `json:"to"`
}

func (CastExpr) isExpr() {}

func (CastExpr) ExprType() string { return "cast" }

type CrossingExprExists struct {
	Kind string `json:"kind"`
	Node string `json:"node"`
}

func (CrossingExprExists) isExpr() {}

func (CrossingExprExists) ExprType() string { return "exists" }

type CrossingExprFirst struct {
	Kind string `json:"kind"`
	Node string `json:"node"`
}

func (CrossingExprFirst) isExpr() {}

func (CrossingExprFirst) ExprType() string { return "first" }

type CrossingExprScalar struct {
	Kind string `json:"kind"`
	Node string `json:"node"`
}

func (CrossingExprScalar) isExpr() {}

func (CrossingExprScalar) ExprType() string { return "scalar" }

type CrossingExprArray struct {
	Kind string `json:"kind"`
	Node string `json:"node"`
}

func (CrossingExprArray) isExpr() {}

func (CrossingExprArray) ExprType() string { return "array" }

type AggTerm struct {
	Arg *Expr  `json:"arg,omitempty"`
	As  string `json:"as"`
	Fn  string `json:"fn"`
}

type Field struct {
	As   string `json:"as"`
	Expr Expr   `json:"expr"`
}

type GroupTerm struct {
	As   *string `json:"as,omitempty"`
	Expr Expr    `json:"expr"`
}

type OrderTerm struct {
	Desc *bool `json:"desc,omitempty"`
	Expr Expr  `json:"expr"`
}

// A strict, single-consumer relation tree encoded as named inline node definitions plus a root selector. The normative semantics live in docs/design/lir.md.
type NodeUnion interface {
	NodeType() string
	isNode()
}

type Node struct {
	NodeUnion
}

func (w Node) MarshalJSON() ([]byte, error) {
	if w.NodeUnion == nil {
		return []byte("null"), nil
	}
	return json.Marshal(w.NodeUnion)
}

func (w *Node) UnmarshalJSON(data []byte) error {
	data = bytes.TrimSpace(data)
	if bytes.Equal(data, []byte("null")) {
		w.NodeUnion = nil
		return nil
	}

	var peek struct {
		Type string `json:"kind"`
	}
	if err := json.Unmarshal(data, &peek); err != nil {
		return fmt.Errorf("Node: invalid JSON: %w", err)
	}
	if peek.Type == "" {
		return fmt.Errorf("Node: missing discriminator field %q", "kind")
	}

	var v NodeUnion
	switch peek.Type {
	case "scan":
		v = &ScanNode{}
	case "filter":
		v = &FilterNode{}
	case "project":
		v = &ProjectNode{}
	case "join":
		v = &JoinNode{}
	case "aggregate":
		v = &AggregateNode{}
	case "order":
		v = &OrderNode{}
	case "slice":
		v = &SliceNode{}
	default:
		return fmt.Errorf("Node: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("Node: invalid %q payload: %w", peek.Type, err)
	}

	w.NodeUnion = v
	return nil
}

type ScanNode struct {
	Kind  string `json:"kind"`
	Scope string `json:"scope"`
	Table string `json:"table"`
}

func (ScanNode) isNode() {}

func (ScanNode) NodeType() string { return "scan" }

type FilterNode struct {
	Input     string `json:"input"`
	Kind      string `json:"kind"`
	Predicate Expr   `json:"predicate"`
}

func (FilterNode) isNode() {}

func (FilterNode) NodeType() string { return "filter" }

type ProjectNode struct {
	Fields []Field  `json:"fields,omitempty"`
	Input  string   `json:"input"`
	Kind   string   `json:"kind"`
	Scope  *string  `json:"scope,omitempty"`
	Spread []string `json:"spread,omitempty"`
}

func (ProjectNode) isNode() {}

func (ProjectNode) NodeType() string { return "project" }

type JoinNode struct {
	Join  string `json:"join"`
	Kind  string `json:"kind"`
	Left  string `json:"left"`
	On    Expr   `json:"on"`
	Right string `json:"right"`
}

func (JoinNode) isNode() {}

func (JoinNode) NodeType() string { return "join" }

type AggregateNode struct {
	Aggs   []AggTerm   `json:"aggs,omitempty"`
	Groups []GroupTerm `json:"groups,omitempty"`
	Input  string      `json:"input"`
	Kind   string      `json:"kind"`
	Scope  *string     `json:"scope,omitempty"`
}

func (AggregateNode) isNode() {}

func (AggregateNode) NodeType() string { return "aggregate" }

type OrderNode struct {
	Input string      `json:"input"`
	Kind  string      `json:"kind"`
	Terms []OrderTerm `json:"terms"`
}

func (OrderNode) isNode() {}

func (OrderNode) NodeType() string { return "order" }

type SliceNode struct {
	Input  string `json:"input"`
	Kind   string `json:"kind"`
	Limit  *int   `json:"limit,omitempty"`
	Offset *int   `json:"offset,omitempty"`
}

func (SliceNode) isNode() {}

func (SliceNode) NodeType() string { return "slice" }

// Selects the root relation and its materialisation mode. Scalar requires one output column and statically at most one row.
type Root struct {
	Cardinality string `json:"cardinality"`
	Node        string `json:"node"`
}

type Query struct {
	// A flat encoding of one finite, single-consumer relation tree. Node IDs name inline definitions and carry no sharing identity.
	Nodes map[string]Node `json:"nodes"`
	Root  Root            `json:"root"`
}
