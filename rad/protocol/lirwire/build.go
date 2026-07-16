package lirwire

// Hand-written, colocated with the Schemancer-generated LIR wire types. These
// are the ergonomic construction surface — validating constructors that return
// the generated union structs directly, so there is one wire representation
// (no parallel "protocol" IR, no conversion bridge). Clients, graphconv, and
// the generative/metamorphic test suite all build LIR through these.
//
// Union members are stored as pointers, matching the generated UnmarshalJSON:
// a built node and a decoded node are the same concrete type, so a type switch
// over the union is uniform and build -> marshal -> decode round-trips.
//
// Regeneration rewrites only lirwire.go (the generated file), never this one.

import (
	"encoding/json"
	"strconv"
)

// -
// relation nodes
// -

func Scan(table, scope string) Node {
	return Node{&ScanNode{Kind: "scan", Table: table, Scope: scope}}
}

func Rows(scope string, columns []RowsColumn, rows [][]Value) Node {
	return Node{&RowsNode{Kind: "rows", Scope: scope, Columns: columns, Rows: rows}}
}

func Filter(input string, predicate Expr) Node {
	return Node{&FilterNode{Kind: "filter", Input: input, Predicate: predicate}}
}

// Project builds a projection. An empty scope means no output label; empty
// spread and fields slices are omitted.
func Project(input, scope string, spread []string, fields []Field) Node {
	n := &ProjectNode{Kind: "project", Input: input, Spread: spread, Fields: fields}
	if scope != "" {
		n.Scope = &scope
	}
	return Node{n}
}

func Join(left, right, join string, on Expr) Node {
	return Node{&JoinNode{Kind: "join", Left: left, Right: right, Join: join, On: on}}
}

// Aggregate builds a fold. An empty scope means no output label.
func Aggregate(input, scope string, groups []GroupTerm, aggs []AggTerm) Node {
	n := &AggregateNode{Kind: "aggregate", Input: input, Groups: groups, Aggs: aggs}
	if scope != "" {
		n.Scope = &scope
	}
	return Node{n}
}

func Order(input string, terms []OrderTerm) Node {
	return Node{&OrderNode{Kind: "order", Input: input, Terms: terms}}
}

// Slice builds a positional window. offset < 0 omits the offset (default 0);
// a nil limit means unlimited.
func Slice(input string, offset int, limit *int) Node {
	n := &SliceNode{Kind: "slice", Input: input, Limit: limit}
	if offset > 0 {
		n.Offset = &offset
	}
	return Node{n}
}

func Ref(binding, scope string) Node {
	return Node{&RefNode{Kind: "ref", Binding: binding, Scope: scope}}
}

// -
// expressions
// -

func Lit(value Value) Expr { return Expr{&LiteralExpr{Kind: "lit", Value: value}} }

func Col(scope, column string) Expr {
	return Expr{&ColumnExpr{Kind: "col", Scope: scope, Column: column}}
}
func Unary(op string, e Expr) Expr { return Expr{&UnaryExpr{Kind: "unary", Op: op, Expr: e}} }
func Binary(op string, l, r Expr) Expr {
	return Expr{&BinaryExpr{Kind: "binary", Op: op, Left: l, Right: r}}
}
func Cast(e Expr, to string) Expr { return Expr{&CastExpr{Kind: "cast", Expr: e, To: to}} }
func Exists(node string) Expr     { return Expr{&CrossingExprExists{Kind: "exists", Node: node}} }
func First(node string) Expr      { return Expr{&CrossingExprFirst{Kind: "first", Node: node}} }
func Scalar(node string) Expr     { return Expr{&CrossingExprScalar{Kind: "scalar", Node: node}} }
func Array(node string) Expr      { return Expr{&CrossingExprArray{Kind: "array", Node: node}} }

// LitOf builds a literal expression from an arbitrary Go value, marshalling it
// to the raw Value form. It is the general path where the concrete type is not
// known at the call site; it cannot fail for a JSON-encodable scalar. Prefer
// Lit with a typed SetX helper where the type is known.
func LitOf(v any) Expr {
	val, _ := SetAny(v)
	return Lit(val)
}

// AndAll left-folds predicates into a binary and-chain: the zero Expr for
// none (a nil union, which marshals to JSON null), the predicate itself for
// one. A caller filtering on an empty predicate set should test IsZero.
func AndAll(preds []Expr) Expr {
	var out Expr
	for i, p := range preds {
		if i == 0 {
			out = p
		} else {
			out = Binary("and", out, p)
		}
	}
	return out
}

// IsZero reports whether e carries no expression (the zero value). AndAll over
// no predicates returns such an Expr.
func (e Expr) IsZero() bool { return e.ExprUnion == nil }

// -
// Value (raw JSON scalar) helpers
// -
//
// Value is a raw-JSON []byte, which is awkward to hand-build. These format
// the four scalar kinds by hand.

func SetString(s string) Value {
	b, _ := json.Marshal(s)
	return Value(b)
}

func SetInt(i int64) Value { return Value(strconv.FormatInt(i, 10)) }

func SetFloat(f float64) Value {
	b, _ := json.Marshal(f)
	return Value(b)
}

func SetBool(b bool) Value {
	if b {
		return Value("true")
	}
	return Value("false")
}

func SetNull() Value { return Value("null") }

// SetAny formats an arbitrary Go value as a Value by marshalling it to JSON.
// It is the general path for a typed value whose concrete type is not known at
// the call site (a fluent client's column literal), preserving json.Number
// precision. Prefer the typed SetX helpers where the type is known.
func SetAny(v any) (Value, error) {
	b, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	return Value(b), nil
}
