package lirwire

// Hand-written, colocated with the Schemancer-generated LIR wire types. These
// are the ergonomic construction surface — validating constructors that return
// the generated union structs directly, so there is one wire representation
// (no parallel "protocol" IR, no conversion bridge). Clients, graphconv, and
// the generative/metamorphic test suite all build LIR through these.
//
// Union members are stored as pointers, matching the generated UnmarshalJSON:
// a built node and a decoded node are the same concrete type, so a type switch
// over the union is uniform and build → marshal → decode round-trips.
//
// Regeneration rewrites only lirwire.go (the generated file), never this one.

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
)

// -
// relation nodes
// -

func Scan(table, scope string) Node {
	return Node{&ScanNode{Kind: "scan", Table: table, Scope: scope}}
}

func Rows(scope string, columns []RowsColumn, rows [][]Cell) Node {
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
func Cast(e Expr, to ScalarType) Expr { return Expr{&CastExpr{Kind: "cast", Expr: e, To: to}} }
func Exists(node string) Expr     { return Expr{&CrossingExprExists{Kind: "exists", Node: node}} }
func First(node string) Expr      { return Expr{&CrossingExprFirst{Kind: "first", Node: node}} }
func Scalar(node string) Expr     { return Expr{&CrossingExprScalar{Kind: "scalar", Node: node}} }
func Array(node string) Expr      { return Expr{&CrossingExprArray{Kind: "array", Node: node}} }

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
// scalar values (the self-describing Value union)
// -
//
// A Value is a tagged scalar for a context that has no adjacent column schema —
// a literal. Its non-NULL payload is a lossless string (numbers keep full
// precision), except bool, which is a native JSON boolean; a nil payload is a
// typed NULL. Rows cells use Cell (below) instead, since their column already
// declares the type.

func Text(s string) Value { return Value{&TextValue{Type: "text", Value: &s}} }

func Int64(i int64) Value {
	s := strconv.FormatInt(i, 10)
	return Value{&Int64Value{Type: "int64", Value: &s}}
}

func Float64(f float64) Value {
	s := strconv.FormatFloat(f, 'g', -1, 64)
	return Value{&Float64Value{Type: "float64", Value: &s}}
}

func Bool(b bool) Value { return Value{&BoolValue{Type: "bool", Value: &b}} }

// Null builds a typed NULL of the given scalar type.
func Null(kind ScalarType) Value {
	switch kind {
	case ScalarTypeText:
		return Value{&TextValue{Type: "text"}}
	case ScalarTypeInt64:
		return Value{&Int64Value{Type: "int64"}}
	case ScalarTypeFloat64:
		return Value{&Float64Value{Type: "float64"}}
	case ScalarTypeBool:
		return Value{&BoolValue{Type: "bool"}}
	}
	return Value{}
}

// LitOf builds a literal expression from a Go scalar, choosing the Value
// variant by dynamic type. It cannot type a nil (an untyped NULL has no scalar
// type) — write Lit(Null(kind)) for a typed NULL literal.
func LitOf(v any) Expr {
	switch x := v.(type) {
	case string:
		return Lit(Text(x))
	case int:
		return Lit(Int64(int64(x)))
	case int64:
		return Lit(Int64(x))
	case float64:
		return Lit(Float64(x))
	case bool:
		return Lit(Bool(x))
	case json.Number:
		// A JSON-decoded number of unknown kind. Keep its lexeme verbatim
		// (a large int can outrun float64), choosing the variant by form.
		s := x.String()
		if strings.ContainsAny(s, ".eE") {
			return Lit(Value{&Float64Value{Type: "float64", Value: &s}})
		}
		return Lit(Value{&Int64Value{Type: "int64", Value: &s}})
	}
	return Lit(Value{})
}

// -
// rows cells (schema-directed payloads)
// -
//
// A Cell is one payload in a rows relation, decoded against its column's
// declared scalar type. It is a *string: nil is a typed NULL; otherwise the
// lossless string form (a bool cell is "true"/"false", not a JSON boolean).

// Cell encodes a Go scalar as a rows cell for a column of the given type. A nil
// value is a NULL. The value's Go type must suit the column type: string for
// text, int/int64/json.Number for int64, int/int64/float64/json.Number for
// float64 (a JSON-decoded value arrives as json.Number), bool for bool.
func MakeCell(kind ScalarType, v any) (Cell, error) {
	if v == nil {
		return nil, nil
	}
	var s string
	switch kind {
	case ScalarTypeText:
		t, ok := v.(string)
		if !ok {
			return nil, fmt.Errorf("lirwire: text cell needs a string, got %T", v)
		}
		s = t
	case ScalarTypeInt64:
		switch n := v.(type) {
		case int64:
			s = strconv.FormatInt(n, 10)
		case int:
			s = strconv.FormatInt(int64(n), 10)
		case json.Number:
			i, err := n.Int64()
			if err != nil {
				return nil, fmt.Errorf("lirwire: int64 cell %q: %w", n.String(), err)
			}
			s = strconv.FormatInt(i, 10)
		default:
			return nil, fmt.Errorf("lirwire: int64 cell needs an integer, got %T", v)
		}
	case ScalarTypeFloat64:
		switch n := v.(type) {
		case float64:
			s = strconv.FormatFloat(n, 'g', -1, 64)
		case int:
			s = strconv.FormatFloat(float64(n), 'g', -1, 64)
		case int64:
			s = strconv.FormatFloat(float64(n), 'g', -1, 64)
		case json.Number:
			f, err := n.Float64()
			if err != nil {
				return nil, fmt.Errorf("lirwire: float64 cell %q: %w", n.String(), err)
			}
			s = strconv.FormatFloat(f, 'g', -1, 64)
		default:
			return nil, fmt.Errorf("lirwire: float64 cell needs a number, got %T", v)
		}
	case ScalarTypeBool:
		b, ok := v.(bool)
		if !ok {
			return nil, fmt.Errorf("lirwire: bool cell needs a boolean, got %T", v)
		}
		s = strconv.FormatBool(b)
	default:
		return nil, fmt.Errorf("lirwire: unknown scalar type %q", kind)
	}
	return &s, nil
}
