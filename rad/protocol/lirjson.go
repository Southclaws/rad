package protocol

import (
	"bytes"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"

	"github.com/google/jsonschema-go/jsonschema"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// lir.schema.yaml is the authored source of truth. schemagen converts it to
// lir.schema.json (embedded below, read by Schemancer) and publishes a copy to
// the website route so the $id URL resolves.
//
//go:generate go run github.com/Southclaws/rad/tools/schemagen -yaml lir.schema.yaml -json lir.schema.json -web ../../home/public/schema/lir.json

// lirSchemaJSON is the independent LIR contract. OpenAPI deliberately treats
// query bodies as raw JSON and does not import this schema.
//
//go:embed lir.schema.json
var lirSchemaJSON []byte

var resolveLIRSchema = sync.OnceValues(func() (*jsonschema.Resolved, error) {
	var schema jsonschema.Schema
	if err := json.Unmarshal(lirSchemaJSON, &schema); err != nil {
		return nil, fmt.Errorf("protocol: decode embedded LIR schema: %w", err)
	}
	resolved, err := schema.Resolve(nil)
	if err != nil {
		return nil, fmt.Errorf("protocol: resolve embedded LIR schema: %w", err)
	}
	return resolved, nil
})

// ValidateLIRJSON validates one raw JSON document against the current LIR
// schema. Literal bytes are decoded separately by the generated wire types, so
// schema validation never becomes the source of transported numeric values.
func ValidateLIRJSON(raw []byte) error {
	dec := json.NewDecoder(bytes.NewReader(raw))
	var instance any
	if err := dec.Decode(&instance); err != nil {
		return fmt.Errorf("protocol: invalid LIR JSON: %w", err)
	}
	if err := ensureJSONEOF(dec); err != nil {
		return err
	}
	resolved, err := resolveLIRSchema()
	if err != nil {
		return err
	}
	if err := resolved.Validate(instance); err != nil {
		return validationDetail(instance, fmt.Errorf("protocol: LIR schema validation failed: %w", err))
	}
	return nil
}

func ensureJSONEOF(dec *json.Decoder) error {
	var extra any
	err := dec.Decode(&extra)
	if errors.Is(err, io.EOF) {
		return nil
	}
	if err == nil {
		return fmt.Errorf("protocol: invalid LIR JSON: multiple JSON values")
	}
	return fmt.Errorf("protocol: invalid LIR JSON: %w", err)
}

// MarshalQuery converts the ergonomic query builder model to the
// Schemancer-generated wire model and returns a schema-valid JSON document.
func MarshalQuery(q Query) ([]byte, error) {
	w, err := queryToWire(q)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(w)
	if err != nil {
		return nil, fmt.Errorf("protocol: encode LIR: %w", err)
	}
	if err := ValidateLIRJSON(raw); err != nil {
		return nil, err
	}
	return raw, nil
}

// UnmarshalQuery validates raw LIR before decoding the generated tagged
// unions, then converts them to the ergonomic query builder model.
func UnmarshalQuery(raw []byte) (Query, error) {
	if err := ValidateLIRJSON(raw); err != nil {
		return Query{}, err
	}
	var w lirwire.Query
	if err := json.Unmarshal(raw, &w); err != nil {
		return Query{}, fmt.Errorf("protocol: decode generated LIR types: %w", err)
	}
	return queryFromWire(w)
}

func queryToWire(q Query) (lirwire.Query, error) {
	nodes := make(map[string]lirwire.Node, len(q.Nodes))
	for name, node := range q.Nodes {
		w, err := nodeToWire(node)
		if err != nil {
			return lirwire.Query{}, fmt.Errorf("protocol: node %q: %w", name, err)
		}
		nodes[name] = w
	}
	var bindings map[string]lirwire.Binding
	if len(q.Bindings) > 0 {
		bindings = make(map[string]lirwire.Binding, len(q.Bindings))
		for name, b := range q.Bindings {
			bindings[name] = lirwire.Binding{Node: b.Node}
		}
	}
	return lirwire.Query{
		Nodes:    nodes,
		Bindings: bindings,
		Root:     lirwire.Root{Node: q.Root.Node, Cardinality: q.Root.Cardinality},
	}, nil
}

func queryFromWire(w lirwire.Query) (Query, error) {
	nodes := make(map[string]Node, len(w.Nodes))
	for name, node := range w.Nodes {
		n, err := nodeFromWire(node)
		if err != nil {
			return Query{}, fmt.Errorf("protocol: node %q: %w", name, err)
		}
		nodes[name] = n
	}
	var bindings map[string]Binding
	if len(w.Bindings) > 0 {
		bindings = make(map[string]Binding, len(w.Bindings))
		for name, b := range w.Bindings {
			bindings[name] = Binding{Node: b.Node}
		}
	}
	return Query{Nodes: nodes, Bindings: bindings, Root: Root{Node: w.Root.Node, Cardinality: w.Root.Cardinality}}, nil
}

func nodeToWire(n Node) (lirwire.Node, error) {
	var u lirwire.NodeUnion
	switch n.Kind {
	case "scan":
		u = &lirwire.ScanNode{Kind: "scan", Table: n.Table, Scope: n.Scope}
	case "rows":
		cols := make([]lirwire.RowsColumn, len(n.Columns))
		for i, c := range n.Columns {
			cols[i] = lirwire.RowsColumn{Name: c.Name, Type: c.Type, Nullable: optionalBool(c.Nullable)}
		}
		rows := make([][]lirwire.Value, len(n.Rows))
		for i, row := range n.Rows {
			cells := make([]lirwire.Value, len(row))
			for j, cell := range row {
				raw, err := json.Marshal(cell)
				if err != nil {
					return lirwire.Node{}, fmt.Errorf("encode rows cell [%d][%d]: %w", i, j, err)
				}
				cells[j] = raw
			}
			rows[i] = cells
		}
		u = &lirwire.RowsNode{Kind: "rows", Scope: n.Scope, Columns: cols, Rows: rows}
	case "filter":
		pred, err := exprToWire(n.Predicate)
		if err != nil {
			return lirwire.Node{}, err
		}
		u = &lirwire.FilterNode{Kind: "filter", Input: n.Input, Predicate: pred}
	case "project":
		fields := make([]lirwire.Field, len(n.Fields))
		for i, field := range n.Fields {
			expr, err := exprToWire(&field.Expr)
			if err != nil {
				return lirwire.Node{}, err
			}
			fields[i] = lirwire.Field{As: field.As, Expr: expr}
		}
		u = &lirwire.ProjectNode{Kind: "project", Input: n.Input, Scope: optionalString(n.Scope), Spread: n.Spread, Fields: fields}
	case "join":
		on, err := exprToWire(n.On)
		if err != nil {
			return lirwire.Node{}, err
		}
		u = &lirwire.JoinNode{Kind: "join", Left: n.Left, Right: n.Right, Join: n.Join, On: on}
	case "aggregate":
		groups := make([]lirwire.GroupTerm, len(n.Groups))
		for i, group := range n.Groups {
			expr, err := exprToWire(&group.Expr)
			if err != nil {
				return lirwire.Node{}, err
			}
			groups[i] = lirwire.GroupTerm{As: optionalString(group.As), Expr: expr}
		}
		aggs := make([]lirwire.AggTerm, len(n.Aggs))
		for i, agg := range n.Aggs {
			var arg *lirwire.Expr
			if agg.Arg != nil {
				expr, err := exprToWire(agg.Arg)
				if err != nil {
					return lirwire.Node{}, err
				}
				arg = &expr
			}
			aggs[i] = lirwire.AggTerm{Fn: agg.Fn, Arg: arg, As: agg.As}
		}
		u = &lirwire.AggregateNode{Kind: "aggregate", Input: n.Input, Scope: optionalString(n.Scope), Groups: groups, Aggs: aggs}
	case "order":
		terms := make([]lirwire.OrderTerm, len(n.Terms))
		for i, term := range n.Terms {
			expr, err := exprToWire(&term.Expr)
			if err != nil {
				return lirwire.Node{}, err
			}
			terms[i] = lirwire.OrderTerm{Expr: expr, Desc: optionalBool(term.Desc)}
		}
		u = &lirwire.OrderNode{Kind: "order", Input: n.Input, Terms: terms}
	case "slice":
		u = &lirwire.SliceNode{Kind: "slice", Input: n.Input, Offset: n.Offset, Limit: n.Limit}
	case "ref":
		u = &lirwire.RefNode{Kind: "ref", Binding: n.Binding, Scope: n.Scope}
	default:
		return lirwire.Node{}, fmt.Errorf("unknown relation kind %q", n.Kind)
	}
	return lirwire.Node{NodeUnion: u}, nil
}

func nodeFromWire(n lirwire.Node) (Node, error) {
	switch x := n.NodeUnion.(type) {
	case *lirwire.ScanNode:
		return Node{Kind: "scan", Table: x.Table, Scope: x.Scope}, nil
	case *lirwire.RowsNode:
		cols := make([]RowsColumn, len(x.Columns))
		for i, c := range x.Columns {
			cols[i] = RowsColumn{Name: c.Name, Type: c.Type, Nullable: boolValue(c.Nullable)}
		}
		rows := make([][]any, len(x.Rows))
		for i, row := range x.Rows {
			cells := make([]any, len(row))
			for j, cell := range row {
				value, err := decodeRawValue(json.RawMessage(cell))
				if err != nil {
					return Node{}, fmt.Errorf("rows cell [%d][%d]: %w", i, j, err)
				}
				cells[j] = value
			}
			rows[i] = cells
		}
		return Node{Kind: "rows", Scope: x.Scope, Columns: cols, Rows: rows}, nil
	case *lirwire.FilterNode:
		pred, err := exprFromWire(x.Predicate)
		return Node{Kind: "filter", Input: x.Input, Predicate: pred}, err
	case *lirwire.ProjectNode:
		fields := make([]Field, len(x.Fields))
		for i, field := range x.Fields {
			expr, err := exprFromWire(field.Expr)
			if err != nil {
				return Node{}, err
			}
			fields[i] = Field{As: field.As, Expr: *expr}
		}
		return Node{Kind: "project", Input: x.Input, Scope: stringValue(x.Scope), Spread: x.Spread, Fields: fields}, nil
	case *lirwire.JoinNode:
		on, err := exprFromWire(x.On)
		return Node{Kind: "join", Left: x.Left, Right: x.Right, Join: x.Join, On: on}, err
	case *lirwire.AggregateNode:
		groups := make([]GroupTerm, len(x.Groups))
		for i, group := range x.Groups {
			expr, err := exprFromWire(group.Expr)
			if err != nil {
				return Node{}, err
			}
			groups[i] = GroupTerm{As: stringValue(group.As), Expr: *expr}
		}
		aggs := make([]AggTerm, len(x.Aggs))
		for i, agg := range x.Aggs {
			var arg *Expr
			var err error
			if agg.Arg != nil {
				arg, err = exprFromWire(*agg.Arg)
				if err != nil {
					return Node{}, err
				}
			}
			aggs[i] = AggTerm{Fn: agg.Fn, Arg: arg, As: agg.As}
		}
		return Node{Kind: "aggregate", Input: x.Input, Scope: stringValue(x.Scope), Groups: groups, Aggs: aggs}, nil
	case *lirwire.OrderNode:
		terms := make([]OrderTerm, len(x.Terms))
		for i, term := range x.Terms {
			expr, err := exprFromWire(term.Expr)
			if err != nil {
				return Node{}, err
			}
			terms[i] = OrderTerm{Expr: *expr, Desc: boolValue(term.Desc)}
		}
		return Node{Kind: "order", Input: x.Input, Terms: terms}, nil
	case *lirwire.SliceNode:
		return Node{Kind: "slice", Input: x.Input, Offset: x.Offset, Limit: x.Limit}, nil
	case *lirwire.RefNode:
		return Node{Kind: "ref", Binding: x.Binding, Scope: x.Scope}, nil
	default:
		return Node{}, fmt.Errorf("unknown generated relation variant %T", n.NodeUnion)
	}
}

func exprToWire(e *Expr) (lirwire.Expr, error) {
	if e == nil {
		return lirwire.Expr{}, nil
	}
	var u lirwire.ExprUnion
	switch e.Kind {
	case "lit":
		raw, err := json.Marshal(e.Value)
		if err != nil {
			return lirwire.Expr{}, fmt.Errorf("encode literal: %w", err)
		}
		u = &lirwire.LiteralExpr{Kind: "lit", Value: raw}
	case "col":
		u = &lirwire.ColumnExpr{Kind: "col", Scope: e.Scope, Column: e.Column}
	case "unary":
		expr, err := exprToWire(e.Expr)
		if err != nil {
			return lirwire.Expr{}, err
		}
		u = &lirwire.UnaryExpr{Kind: "unary", Op: e.Op, Expr: expr}
	case "binary":
		left, err := exprToWire(e.Left)
		if err != nil {
			return lirwire.Expr{}, err
		}
		right, err := exprToWire(e.Right)
		if err != nil {
			return lirwire.Expr{}, err
		}
		u = &lirwire.BinaryExpr{Kind: "binary", Op: e.Op, Left: left, Right: right}
	case "cast":
		expr, err := exprToWire(e.Expr)
		if err != nil {
			return lirwire.Expr{}, err
		}
		u = &lirwire.CastExpr{Kind: "cast", Expr: expr, To: e.To}
	case "exists":
		u = &lirwire.CrossingExprExists{Kind: "exists", Node: e.Node}
	case "first":
		u = &lirwire.CrossingExprFirst{Kind: "first", Node: e.Node}
	case "scalar":
		u = &lirwire.CrossingExprScalar{Kind: "scalar", Node: e.Node}
	case "array":
		u = &lirwire.CrossingExprArray{Kind: "array", Node: e.Node}
	default:
		return lirwire.Expr{}, fmt.Errorf("unknown expression kind %q", e.Kind)
	}
	return lirwire.Expr{ExprUnion: u}, nil
}

func exprFromWire(e lirwire.Expr) (*Expr, error) {
	switch x := e.ExprUnion.(type) {
	case *lirwire.LiteralExpr:
		value, err := decodeRawValue(x.Value)
		return &Expr{Kind: "lit", Value: value}, err
	case *lirwire.ColumnExpr:
		return &Expr{Kind: "col", Scope: x.Scope, Column: x.Column}, nil
	case *lirwire.UnaryExpr:
		expr, err := exprFromWire(x.Expr)
		return &Expr{Kind: "unary", Op: x.Op, Expr: expr}, err
	case *lirwire.BinaryExpr:
		left, err := exprFromWire(x.Left)
		if err != nil {
			return nil, err
		}
		right, err := exprFromWire(x.Right)
		return &Expr{Kind: "binary", Op: x.Op, Left: left, Right: right}, err
	case *lirwire.CastExpr:
		expr, err := exprFromWire(x.Expr)
		return &Expr{Kind: "cast", Expr: expr, To: x.To}, err
	case *lirwire.CrossingExprExists:
		return &Expr{Kind: "exists", Node: x.Node}, nil
	case *lirwire.CrossingExprFirst:
		return &Expr{Kind: "first", Node: x.Node}, nil
	case *lirwire.CrossingExprScalar:
		return &Expr{Kind: "scalar", Node: x.Node}, nil
	case *lirwire.CrossingExprArray:
		return &Expr{Kind: "array", Node: x.Node}, nil
	default:
		return nil, fmt.Errorf("unknown generated expression variant %T", e.ExprUnion)
	}
}

func decodeRawValue(raw json.RawMessage) (any, error) {
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	var value any
	if err := dec.Decode(&value); err != nil {
		return nil, fmt.Errorf("decode literal: %w", err)
	}
	return value, nil
}

func optionalString(value string) *string {
	if value == "" {
		return nil
	}
	return &value
}

func optionalBool(value bool) *bool {
	if !value {
		return nil
	}
	return &value
}

func stringValue(value *string) string {
	if value == nil {
		return ""
	}
	return *value
}

func boolValue(value *bool) bool {
	return value != nil && *value
}
