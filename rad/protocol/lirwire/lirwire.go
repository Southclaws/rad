package lirwire

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// The logical type of a scalar value: `text`, `int64`, `float64`, or
// `bool`.
type ScalarType string

const (
	ScalarTypeText    ScalarType = "text"
	ScalarTypeInt64   ScalarType = "int64"
	ScalarTypeFloat64 ScalarType = "float64"
	ScalarTypeBool    ScalarType = "bool"
)

var ScalarTypeValues = []ScalarType{
	ScalarTypeText,
	ScalarTypeInt64,
	ScalarTypeFloat64,
	ScalarTypeBool,
}

// LIR is Rad's low-level intermediate representation: the relation tree a
// client sends to `POST /execute`, and the tree the engine binds, plans, and
// executes. This schema is its normative specification. The type definitions
// and the prose in these descriptions are one artifact and move together.
//
// LIR is an internal contract with no external compatibility promise yet; it
// may change while the design is hardened. It is deliberately unstable, but at
// any moment it is exactly what this document says.
//
// # Model
//
// LIR has exactly two categories, and no third "shape" category: shaping is
// projection, and nesting lives in the value model.
//
//   - A **relation** is a possibly empty, possibly many stream of structurally
//     typed rows. Relation operators consume relations and produce relations.
//   - An **expression** computes exactly one typed *datum* (null, a scalar, a
//     row, or an array) in some scope. An expression may consume a relation only
//     through an explicit *crossing* that declares how a stream of rows becomes
//     one datum.
//
// **Relational closure** is a law, not an aspiration: the output of every
// relation operator is itself an ordinary relation whose every attribute is
// addressable by later operators. A `filter` above an `aggregate` references a
// fold's output exactly as it would reference a scanned column.
//
// **Correlation** is a derived property, not an operator. A sub-relation that
// references a scope bound by an enclosing relation is correlated, and is
// evaluated with that enclosing row in scope. Correlation is a semantic
// relationship only: the planner may satisfy it per row, by batching, or by a
// join, as long as the result is identical. A relation becomes a datum only at
// a crossing or the root, never merely by being correlated.
//
// Relations have **bag** semantics: an operator changes multiplicity only when
// that is its job. `aggregate` folds each group to one row, and `distinct`
// converts an input bag into the corresponding set of complete rows; every
// other operator preserves multiplicity.
//
// # Determinism
//
// A key-value scan's encounter order is physical, never logical, and the
// engine's access-path choice must never change results. Wherever a stream of
// rows becomes observable, its order must be explicit. Root `many` results and
// `array` crossings require an `order`; root and crossing `first` permit either
// an `order` or a proof of at most one row. A positive `slice.offset` also
// requires an ordered input. An unordered `limit` is permitted only until a
// later observable boundary imposes its own ordering requirement. `distinct`
// likewise imposes no logical ordering — an observable ordered distinct result
// requires an explicit `order` above it, and an ordered input confers none.
//
// # NULL and three-valued logic
//
// Predicates evaluate in Kleene three-valued logic (K3): `TRUE`, `FALSE`, or
// `UNKNOWN`. Any comparison with a NULL operand is `UNKNOWN`. `filter` and a
// join's `on` keep only rows that evaluate to `TRUE`, never `UNKNOWN`, so
// `not (x = 1)` does not match rows where `x` is NULL; `is_null` is the only
// test that matches a NULL. NULLs are distinct under unique indexes.
//
// # The wire
//
// A query is a flat map of caller-chosen node ids plus a root selector. Every
// definition is inline; a node id is a plain reference carrying no sharing,
// memoisation, or materialisation identity. The graph must be acyclic, every
// node must be reachable from the root and have exactly one consumer, and
// dangling or shared references are rejected before binding. Nodes and
// expressions are closed `oneOf` unions selected by `kind`; unknown fields,
// fields belonging to another variant, and missing required fields are
// rejected. Literal values travel as raw JSON scalars (see `Value`) and are
// typed by the binder from the context each literal meets.
type ValueUnion interface {
	ValueType() string
	isValue()
}

type Value struct {
	ValueUnion
}

func (w Value) MarshalJSON() ([]byte, error) {
	if w.ValueUnion == nil {
		return []byte("null"), nil
	}
	return json.Marshal(w.ValueUnion)
}

func (w *Value) UnmarshalJSON(data []byte) error {
	data = bytes.TrimSpace(data)
	if bytes.Equal(data, []byte("null")) {
		w.ValueUnion = nil
		return nil
	}

	var peek struct {
		Type string `json:"type"`
	}
	if err := json.Unmarshal(data, &peek); err != nil {
		return fmt.Errorf("Value: invalid JSON: %w", err)
	}
	if peek.Type == "" {
		return fmt.Errorf("Value: missing discriminator field %q", "type")
	}

	var v ValueUnion
	switch peek.Type {
	case "text":
		v = &TextValue{}
	case "int64":
		v = &Int64Value{}
	case "float64":
		v = &Float64Value{}
	case "bool":
		v = &BoolValue{}
	default:
		return fmt.Errorf("Value: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("Value: invalid %q payload: %w", peek.Type, err)
	}

	w.ValueUnion = v
	return nil
}

// A text value, or a text NULL when `value` is absent.
type TextValue struct {
	Type string `json:"type"`
	// The text value; absent for a NULL.
	Value *string `json:"value,omitempty"`
}

func (TextValue) isValue() {}

func (TextValue) ValueType() string { return "text" }

// A signed 64-bit integer, or an int64 NULL when `value` is absent. `value`
// is the integer's canonical base-10 string, with no leading plus sign or
// redundant leading zeroes; values outside the signed 64-bit range are
// invalid — a bound the pattern cannot express, so the binder enforces it.
type Int64Value struct {
	Type string `json:"type"`
	// The integer as a canonical decimal string, e.g. "9007199254740993"; absent for a NULL.
	Value *string `json:"value,omitempty"`
}

func (Int64Value) isValue() {}

func (Int64Value) ValueType() string { return "int64" }

// A finite IEEE-754 binary64 value, or a float64 NULL when `value` is
// absent. `value` is a decimal string that round-trips exactly to that
// binary64 value; non-finite values (NaN, Infinity) are rejected by the
// binder.
type Float64Value struct {
	Type string `json:"type"`
	// The finite binary64 value in canonical round-trip string form; absent for a NULL.
	Value *string `json:"value,omitempty"`
}

func (Float64Value) isValue() {}

func (Float64Value) ValueType() string { return "float64" }

// A boolean value, or a bool NULL when `value` is absent.
type BoolValue struct {
	Type string `json:"type"`
	// The boolean value; absent for a NULL.
	Value *bool `json:"value,omitempty"`
}

func (BoolValue) isValue() {}

func (BoolValue) ValueType() string { return "bool" }

// LIR is Rad's low-level intermediate representation: the relation tree a
// client sends to `POST /execute`, and the tree the engine binds, plans, and
// executes. This schema is its normative specification. The type definitions
// and the prose in these descriptions are one artifact and move together.
//
// LIR is an internal contract with no external compatibility promise yet; it
// may change while the design is hardened. It is deliberately unstable, but at
// any moment it is exactly what this document says.
//
// # Model
//
// LIR has exactly two categories, and no third "shape" category: shaping is
// projection, and nesting lives in the value model.
//
//   - A **relation** is a possibly empty, possibly many stream of structurally
//     typed rows. Relation operators consume relations and produce relations.
//   - An **expression** computes exactly one typed *datum* (null, a scalar, a
//     row, or an array) in some scope. An expression may consume a relation only
//     through an explicit *crossing* that declares how a stream of rows becomes
//     one datum.
//
// **Relational closure** is a law, not an aspiration: the output of every
// relation operator is itself an ordinary relation whose every attribute is
// addressable by later operators. A `filter` above an `aggregate` references a
// fold's output exactly as it would reference a scanned column.
//
// **Correlation** is a derived property, not an operator. A sub-relation that
// references a scope bound by an enclosing relation is correlated, and is
// evaluated with that enclosing row in scope. Correlation is a semantic
// relationship only: the planner may satisfy it per row, by batching, or by a
// join, as long as the result is identical. A relation becomes a datum only at
// a crossing or the root, never merely by being correlated.
//
// Relations have **bag** semantics: an operator changes multiplicity only when
// that is its job. `aggregate` folds each group to one row, and `distinct`
// converts an input bag into the corresponding set of complete rows; every
// other operator preserves multiplicity.
//
// # Determinism
//
// A key-value scan's encounter order is physical, never logical, and the
// engine's access-path choice must never change results. Wherever a stream of
// rows becomes observable, its order must be explicit. Root `many` results and
// `array` crossings require an `order`; root and crossing `first` permit either
// an `order` or a proof of at most one row. A positive `slice.offset` also
// requires an ordered input. An unordered `limit` is permitted only until a
// later observable boundary imposes its own ordering requirement. `distinct`
// likewise imposes no logical ordering — an observable ordered distinct result
// requires an explicit `order` above it, and an ordered input confers none.
//
// # NULL and three-valued logic
//
// Predicates evaluate in Kleene three-valued logic (K3): `TRUE`, `FALSE`, or
// `UNKNOWN`. Any comparison with a NULL operand is `UNKNOWN`. `filter` and a
// join's `on` keep only rows that evaluate to `TRUE`, never `UNKNOWN`, so
// `not (x = 1)` does not match rows where `x` is NULL; `is_null` is the only
// test that matches a NULL. NULLs are distinct under unique indexes.
//
// # The wire
//
// A query is a flat map of caller-chosen node ids plus a root selector. Every
// definition is inline; a node id is a plain reference carrying no sharing,
// memoisation, or materialisation identity. The graph must be acyclic, every
// node must be reachable from the root and have exactly one consumer, and
// dangling or shared references are rejected before binding. Nodes and
// expressions are closed `oneOf` unions selected by `kind`; unknown fields,
// fields belonging to another variant, and missing required fields are
// rejected. Literal values travel as raw JSON scalars (see `Value`) and are
// typed by the binder from the context each literal meets.
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

// A constant value. `value` is a scalar wire value (see `Value`); numeric
// values travel as lossless strings, so a 64-bit integer keeps full
// precision on the wire.
type LiteralExpr struct {
	Kind string `json:"kind"`
	// The constant's typed scalar value.
	Value Value `json:"value"`
}

func (LiteralExpr) isExpr() {}

func (LiteralExpr) ExprType() string { return "lit" }

// Reference a column of a bound scope: `scope` names a `scan` (or a
// `project`/`aggregate` that bound an output scope), and `column` names one
// of its attributes. The referenced scope must be in scope at this point in
// the graph; the binder resolves the reference to the producing relation's
// output. A reference to a scope bound by an enclosing relation is what
// makes a sub-relation correlated.
type ColumnExpr struct {
	// The attribute name within that scope.
	Column string `json:"column"`
	Kind   string `json:"kind"`
	// The label of the relation whose column is referenced.
	Scope string `json:"scope"`
}

func (ColumnExpr) isExpr() {}

func (ColumnExpr) ExprType() string { return "col" }

// Apply a unary operator to `expr`:
//
//   - `not` — three-valued negation: `not TRUE` is FALSE, `not FALSE` is
//     TRUE, `not UNKNOWN` is UNKNOWN.
//   - `negate` — arithmetic negation of a numeric value; NULL propagates.
//   - `is_null` / `is_not_null` — total tests that are always TRUE or FALSE,
//     never UNKNOWN. `is_null` is the only way to match a NULL.
type UnaryExpr struct {
	// The operand.
	Expr Expr   `json:"expr"`
	Kind string `json:"kind"`
	// The unary operator.
	Op string `json:"op"`
}

func (UnaryExpr) isExpr() {}

func (UnaryExpr) ExprType() string { return "unary" }

// Apply a binary operator to `left` and `right`.
//
//   - Comparisons `eq ne lt lte gt gte` yield a boolean under three-valued
//     logic: any NULL operand makes the result `UNKNOWN`.
//   - Logical `and` / `or` follow Kleene logic (`F and x = F`, `T or x = T`,
//     otherwise `UNKNOWN`). They are strictly binary; a client left-folds a
//     longer chain into nested `and`/`or`.
//   - Arithmetic `add sub mul div` operate on numeric operands and propagate
//     NULL. They are part of the grammar and are evaluated, though the
//     generated clients do not yet emit them.
type BinaryExpr struct {
	Kind string `json:"kind"`
	// The left operand.
	Left Expr `json:"left"`
	// The binary operator.
	Op string `json:"op"`
	// The right operand.
	Right Expr `json:"right"`
}

func (BinaryExpr) isExpr() {}

func (BinaryExpr) ExprType() string { return "binary" }

// Convert `expr` to the scalar type `to`. The conversion is explicit; the
// result is nullable exactly when the operand is.
type CastExpr struct {
	// The expression to convert.
	Expr Expr   `json:"expr"`
	Kind string `json:"kind"`
	// The target scalar type.
	To ScalarType `json:"to"`
}

func (CastExpr) isExpr() {}

func (CastExpr) ExprType() string { return "cast" }

// `EXISTS`: a boolean that is `TRUE` when the relation `node` produces at
// least one row and `FALSE` otherwise. Never NULL. The relation may be
// correlated with the enclosing row.
type CrossingExprExists struct {
	Kind string `json:"kind"`
	// The id of the relation whose non-emptiness is tested.
	Node string `json:"node"`
}

func (CrossingExprExists) isExpr() {}

func (CrossingExprExists) ExprType() string { return "exists" }

// The first row of the relation `node` as a nested object, or null when it
// has no rows. Renders a to-one relationship.
//
// Subject to the determinism rule: `first` is accepted only when the
// relation is provably at most one row (a unique-key filter, a global
// aggregate, a `slice` of one) or carries an explicit `order`. Taking the
// first row of an unordered multi-row relation would leak the access path
// into the result and is rejected at bind time.
type CrossingExprFirst struct {
	Kind string `json:"kind"`
	// The id of the relation to take the first row of.
	Node string `json:"node"`
}

func (CrossingExprFirst) isExpr() {}

func (CrossingExprFirst) ExprType() string { return "first" }

// The single value of a single-column, at-most-one-row relation `node`, or
// null when it has no rows. Renders a scalar subquery, such as a correlated
// aggregate count.
//
// `scalar` is a cardinality assertion: the relation must have exactly one
// output column and be statically at most one row, both checked at bind
// time. "First scalar" is not implied; write it as the composition of
// `scalar` over a `slice` of one over an `order`.
type CrossingExprScalar struct {
	Kind string `json:"kind"`
	// The id of the single-column, at-most-one-row relation.
	Node string `json:"node"`
}

func (CrossingExprScalar) isExpr() {}

func (CrossingExprScalar) ExprType() string { return "scalar" }

// Every row of the relation `node` as a nested array, in the relation's
// order. Empty when the relation has no rows, never null. Renders a to-many
// relationship. The relation may be correlated with the enclosing row.
type CrossingExprArray struct {
	Kind string `json:"kind"`
	// The id of the relation to materialise as an array.
	Node string `json:"node"`
}

func (CrossingExprArray) isExpr() {}

func (CrossingExprArray) ExprType() string { return "array" }

// One aggregate fold, exposed in the output under `as`.
//
//   - `count` with no `arg` counts rows; with an `arg` it counts the rows
//     where the argument is non-null. It is int64 and never NULL (`0` over an
//     empty group).
//   - `sum` and `avg` require a numeric argument; `avg` is float64.
//   - `min` and `max` accept any comparable argument.
//
// All folds skip NULL arguments. Over an empty global fold, `count` is `0`
// and every other fold is NULL.
type AggTerm struct {
	// The argument expression. Omitted only for `count`, which then counts
	// rows rather than non-null values.
	//
	Arg *Expr `json:"arg,omitempty"`
	// The output attribute name for this fold.
	As string `json:"as"`
	// The fold function.
	Fn string `json:"fn"`
}

// LIR is Rad's low-level intermediate representation: the relation tree a
// client sends to `POST /execute`, and the tree the engine binds, plans, and
// executes. This schema is its normative specification. The type definitions
// and the prose in these descriptions are one artifact and move together.
//
// LIR is an internal contract with no external compatibility promise yet; it
// may change while the design is hardened. It is deliberately unstable, but at
// any moment it is exactly what this document says.
//
// # Model
//
// LIR has exactly two categories, and no third "shape" category: shaping is
// projection, and nesting lives in the value model.
//
//   - A **relation** is a possibly empty, possibly many stream of structurally
//     typed rows. Relation operators consume relations and produce relations.
//   - An **expression** computes exactly one typed *datum* (null, a scalar, a
//     row, or an array) in some scope. An expression may consume a relation only
//     through an explicit *crossing* that declares how a stream of rows becomes
//     one datum.
//
// **Relational closure** is a law, not an aspiration: the output of every
// relation operator is itself an ordinary relation whose every attribute is
// addressable by later operators. A `filter` above an `aggregate` references a
// fold's output exactly as it would reference a scanned column.
//
// **Correlation** is a derived property, not an operator. A sub-relation that
// references a scope bound by an enclosing relation is correlated, and is
// evaluated with that enclosing row in scope. Correlation is a semantic
// relationship only: the planner may satisfy it per row, by batching, or by a
// join, as long as the result is identical. A relation becomes a datum only at
// a crossing or the root, never merely by being correlated.
//
// Relations have **bag** semantics: an operator changes multiplicity only when
// that is its job. `aggregate` folds each group to one row, and `distinct`
// converts an input bag into the corresponding set of complete rows; every
// other operator preserves multiplicity.
//
// # Determinism
//
// A key-value scan's encounter order is physical, never logical, and the
// engine's access-path choice must never change results. Wherever a stream of
// rows becomes observable, its order must be explicit. Root `many` results and
// `array` crossings require an `order`; root and crossing `first` permit either
// an `order` or a proof of at most one row. A positive `slice.offset` also
// requires an ordered input. An unordered `limit` is permitted only until a
// later observable boundary imposes its own ordering requirement. `distinct`
// likewise imposes no logical ordering — an observable ordered distinct result
// requires an explicit `order` above it, and an ordered input confers none.
//
// # NULL and three-valued logic
//
// Predicates evaluate in Kleene three-valued logic (K3): `TRUE`, `FALSE`, or
// `UNKNOWN`. Any comparison with a NULL operand is `UNKNOWN`. `filter` and a
// join's `on` keep only rows that evaluate to `TRUE`, never `UNKNOWN`, so
// `not (x = 1)` does not match rows where `x` is NULL; `is_null` is the only
// test that matches a NULL. NULLs are distinct under unique indexes.
//
// # The wire
//
// A query is a flat map of caller-chosen node ids plus a root selector. Every
// definition is inline; a node id is a plain reference carrying no sharing,
// memoisation, or materialisation identity. The graph must be acyclic, every
// node must be reachable from the root and have exactly one consumer, and
// dangling or shared references are rejected before binding. Nodes and
// expressions are closed `oneOf` unions selected by `kind`; unknown fields,
// fields belonging to another variant, and missing required fields are
// rejected. Literal values travel as raw JSON scalars (see `Value`) and are
// typed by the binder from the context each literal meets.
type BindingUnion interface {
	BindingType() string
	isBinding()
}

type Binding struct {
	BindingUnion
}

func (w Binding) MarshalJSON() ([]byte, error) {
	if w.BindingUnion == nil {
		return []byte("null"), nil
	}
	return json.Marshal(w.BindingUnion)
}

func (w *Binding) UnmarshalJSON(data []byte) error {
	data = bytes.TrimSpace(data)
	if bytes.Equal(data, []byte("null")) {
		w.BindingUnion = nil
		return nil
	}

	var peek struct {
		Type string `json:"kind"`
	}
	if err := json.Unmarshal(data, &peek); err != nil {
		return fmt.Errorf("Binding: invalid JSON: %w", err)
	}
	if peek.Type == "" {
		return fmt.Errorf("Binding: missing discriminator field %q", "kind")
	}

	var v BindingUnion
	switch peek.Type {
	case "derived":
		v = &DerivedBinding{}
	case "recursive":
		v = &RecursiveBinding{}
	default:
		return fmt.Errorf("Binding: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("Binding: invalid %q payload: %w", peek.Type, err)
	}

	w.BindingUnion = v
	return nil
}

// A binding whose value is the one committed relational value its defining
// tree produces for this statement. Under bag semantics that value is one
// committed bag: a nondeterministic body (a slice with no total order) is
// resolved once, and every reference observes the same choice. The
// binding's public output is its root's output shape — interior scopes
// never leak.
type DerivedBinding struct {
	Kind string `json:"kind"`
	// The id of the binding's defining root node in `nodes`.
	Node string `json:"node"`
}

func (DerivedBinding) isBinding() {}

func (DerivedBinding) BindingType() string { return "derived" }

// A recursively-defined relation, evaluated by iterating a step over a
// frontier. The `anchor` (base case) is evaluated once to seed both the
// working table and the result; then the `step` (inductive case) is
// evaluated repeatedly with its `recursive_ref` bound to the previous
// iteration's rows — the frontier — and each iteration's output is added to
// the result, until the frontier is empty. `accumulation: new` admits each
// full row at most once: rows already in the accumulated result, and
// duplicates produced within the same iteration, are dropped before the
// next frontier forms (a set fixpoint). `accumulation: all` keeps every
// generated row, multiplicity included. This is recursive admission, not
// the unary `distinct` operator — they share canonical full-row identity
// but are separate constructs. The `step` must contain exactly one
// `recursive_ref` to this binding and the `anchor` none; the reference
// denotes the frontier, never the accumulated result.
//
// The anchor supplies the binding's column names, order, and kinds;
// nullability is the least widening that accommodates both the anchor and
// the recursive step. The relation has no inherent order and termination is
// not guaranteed — observing it requires an explicit `order`, and a
// non-terminating recursion is bounded only by the engine's iteration and
// row limits.
type RecursiveBinding struct {
	// How each iteration's rows enter the result and next frontier: `all`
	// keeps every generated row with its multiplicity; `new` admits each
	// full row at most once, dropping rows already in the accumulated
	// result and duplicates within the same iteration. Recursive admission,
	// not the unary `distinct` operator.
	//
	Accumulation string `json:"accumulation"`
	// The base-case root node. Contains no `recursive_ref`.
	Anchor string `json:"anchor"`
	Kind   string `json:"kind"`
	// The inductive-case root node. Contains exactly one `recursive_ref`
	// to this binding.
	//
	Step string `json:"step"`
}

func (RecursiveBinding) isBinding() {}

func (RecursiveBinding) BindingType() string { return "recursive" }

// One schema-directed scalar payload, used inside a `rows` relation where
// each column already declares its type. A string is decoded against the
// corresponding `RowsColumn.type` — numbers as lossless strings, `bool` as
// "true"/"false", so precision survives and the type is never repeated per
// cell; JSON null is a typed NULL, valid only when that column is nullable.
// Unlike `Value`, a cell carries no type of its own — the column supplies
// it once for the whole column.
type Cell = *string

// One computed output attribute of a projection: the expression `expr`
// under the output name `as`. When `expr` is a crossing the field
// materialises a nested value: `first` renders an object (or null), `array`
// a nested array, `scalar` a bare value, and `exists` a boolean.
type Field struct {
	// The output attribute name. Must not collide with another.
	As string `json:"as"`
	// The expression computing the attribute's value.
	Expr Expr `json:"expr"`
}

// One grouping key of an aggregate: rows are grouped by the value of
// `expr`, and the key is exposed in the output under `as`. When `as` is
// omitted and `expr` is a bare column reference, the column's name is used.
type GroupTerm struct {
	// The output name for this key. Defaults to the referenced column name
	// when `expr` is a bare column.
	//
	As *string `json:"as,omitempty"`
	// The expression whose value defines the group.
	Expr Expr `json:"expr"`
}

// One ordering term: sort by the value of `expr`, ascending unless `desc`
// is true. NULLs sort first when ascending and last when descending. Terms
// apply in list order; a deterministic unique-key tie-breaker is appended
// by the engine when one is known.
type OrderTerm struct {
	// Sort descending instead of the default ascending.
	Desc *bool `json:"desc,omitempty"`
	// The expression to sort by.
	Expr Expr `json:"expr"`
}

// One declared output column of a constant relation.
type RowsColumn struct {
	// The output attribute name.
	Name string `json:"name"`
	// Whether cells in this column may be NULL.
	Nullable *bool `json:"nullable,omitempty"`
	// The column's scalar type.
	Type ScalarType `json:"type"`
}

// LIR is Rad's low-level intermediate representation: the relation tree a
// client sends to `POST /execute`, and the tree the engine binds, plans, and
// executes. This schema is its normative specification. The type definitions
// and the prose in these descriptions are one artifact and move together.
//
// LIR is an internal contract with no external compatibility promise yet; it
// may change while the design is hardened. It is deliberately unstable, but at
// any moment it is exactly what this document says.
//
// # Model
//
// LIR has exactly two categories, and no third "shape" category: shaping is
// projection, and nesting lives in the value model.
//
//   - A **relation** is a possibly empty, possibly many stream of structurally
//     typed rows. Relation operators consume relations and produce relations.
//   - An **expression** computes exactly one typed *datum* (null, a scalar, a
//     row, or an array) in some scope. An expression may consume a relation only
//     through an explicit *crossing* that declares how a stream of rows becomes
//     one datum.
//
// **Relational closure** is a law, not an aspiration: the output of every
// relation operator is itself an ordinary relation whose every attribute is
// addressable by later operators. A `filter` above an `aggregate` references a
// fold's output exactly as it would reference a scanned column.
//
// **Correlation** is a derived property, not an operator. A sub-relation that
// references a scope bound by an enclosing relation is correlated, and is
// evaluated with that enclosing row in scope. Correlation is a semantic
// relationship only: the planner may satisfy it per row, by batching, or by a
// join, as long as the result is identical. A relation becomes a datum only at
// a crossing or the root, never merely by being correlated.
//
// Relations have **bag** semantics: an operator changes multiplicity only when
// that is its job. `aggregate` folds each group to one row, and `distinct`
// converts an input bag into the corresponding set of complete rows; every
// other operator preserves multiplicity.
//
// # Determinism
//
// A key-value scan's encounter order is physical, never logical, and the
// engine's access-path choice must never change results. Wherever a stream of
// rows becomes observable, its order must be explicit. Root `many` results and
// `array` crossings require an `order`; root and crossing `first` permit either
// an `order` or a proof of at most one row. A positive `slice.offset` also
// requires an ordered input. An unordered `limit` is permitted only until a
// later observable boundary imposes its own ordering requirement. `distinct`
// likewise imposes no logical ordering — an observable ordered distinct result
// requires an explicit `order` above it, and an ordered input confers none.
//
// # NULL and three-valued logic
//
// Predicates evaluate in Kleene three-valued logic (K3): `TRUE`, `FALSE`, or
// `UNKNOWN`. Any comparison with a NULL operand is `UNKNOWN`. `filter` and a
// join's `on` keep only rows that evaluate to `TRUE`, never `UNKNOWN`, so
// `not (x = 1)` does not match rows where `x` is NULL; `is_null` is the only
// test that matches a NULL. NULLs are distinct under unique indexes.
//
// # The wire
//
// A query is a flat map of caller-chosen node ids plus a root selector. Every
// definition is inline; a node id is a plain reference carrying no sharing,
// memoisation, or materialisation identity. The graph must be acyclic, every
// node must be reachable from the root and have exactly one consumer, and
// dangling or shared references are rejected before binding. Nodes and
// expressions are closed `oneOf` unions selected by `kind`; unknown fields,
// fields belonging to another variant, and missing required fields are
// rejected. Literal values travel as raw JSON scalars (see `Value`) and are
// typed by the binder from the context each literal meets.
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
	case "rows":
		v = &RowsNode{}
	case "filter":
		v = &FilterNode{}
	case "project":
		v = &ProjectNode{}
	case "join":
		v = &JoinNode{}
	case "concatenate":
		v = &ConcatenateNode{}
	case "intersect":
		v = &IntersectNode{}
	case "except":
		v = &ExceptNode{}
	case "aggregate":
		v = &AggregateNode{}
	case "order":
		v = &OrderNode{}
	case "slice":
		v = &SliceNode{}
	case "ref":
		v = &RefNode{}
	case "recursive_ref":
		v = &RecursiveRefNode{}
	case "distinct":
		v = &DistinctNode{}
	default:
		return fmt.Errorf("Node: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("Node: invalid %q payload: %w", peek.Type, err)
	}

	w.NodeUnion = v
	return nil
}

// Introduce a base table as a relation and bind a `scope` label to its
// rows. Scans introduce stored rows; `rows` introduces literal ones —
// every other operator derives from these leaves.
//
// A scan's row order is physical (whatever the chosen access path yields)
// and never logical: to depend on order, place an explicit `order` above
// the scan. `scope` must be unique across the whole query.
type ScanNode struct {
	Kind string `json:"kind"`
	// The label this scan's rows are bound to, used by `col` references
	// elsewhere in the query. Must be unique across the query.
	//
	Scope string `json:"scope"`
	// The catalog table to scan. Rejected if it does not exist.
	Table string `json:"table"`
}

func (ScanNode) isNode() {}

func (ScanNode) NodeType() string { return "scan" }

// Introduce a finite constant relation: literal rows with explicitly
// declared columns, bound to a `scope` label exactly as a scan is. It is
// the second relational leaf beside `scan`, useful for literal queries,
// joins against ad hoc data, fixtures, and mutation input where a
// literal row is simply a one-row relation.
//
// Column types and nullability are declared, never inferred from the
// row values: a bare JSON number cannot choose between int64 and
// float64, and the schema of a relation must not depend on its data.
// Each row is a positional array parallel to `columns`, and its arity
// must equal the number of declared columns. Each cell is a `Cell`: a bare
// string payload decoded against its column's declared scalar type (numbers
// as lossless strings, so precision survives end to end), or JSON null for a
// typed NULL, valid only for a nullable column. The column supplies the
// type, so — unlike a `Value` literal — a cell never repeats it.
//
// `rows` may be empty. Because type and nullability are declared
// independently of the values, an empty constant relation remains
// fully typed.
//
// The relation is a bag containing exactly `rows.length` rows,
// including duplicates. Document order is not a logical relation
// order; observing an order requires an explicit `order` node above
// it. Its bag contents are deterministic and independent of storage
// access paths or physical plan choice.
type RowsNode struct {
	// The output columns in positional order. Column names must be
	// unique.
	//
	Columns []RowsColumn `json:"columns"`
	Kind    string       `json:"kind"`
	// Literal rows represented as positional arrays parallel to
	// `columns`. May be empty.
	//
	Rows [][]Cell `json:"rows"`
	// The label this relation's output attributes are bound to. Must be
	// unique across the query, under the same rules as a scan's scope.
	//
	Scope string `json:"scope"`
}

func (RowsNode) isNode() {}

func (RowsNode) NodeType() string { return "rows" }

// Keep the rows of `input` whose `predicate` evaluates to `TRUE` under
// three-valued logic. Rows evaluating to `FALSE` or `UNKNOWN` are dropped;
// this is the load-bearing K3 rule. The predicate must be of boolean type.
//
// A filter changes cardinality but not row type: its output is its input's
// row type. A filter whose equality conjuncts cover a unique key of its
// input (a primary key or unique index) is statically at most one row,
// which is what lets the to-parent pattern (`first` over a foreign-key
// match) satisfy the determinism rule.
type FilterNode struct {
	// The id of the relation to filter.
	Input string `json:"input"`
	Kind  string `json:"kind"`
	// A boolean expression; rows for which it is TRUE are kept.
	Predicate Expr `json:"predicate"`
}

func (FilterNode) isNode() {}

func (FilterNode) NodeType() string { return "filter" }

// Establish a new row type from `input`. Projection is not column
// selection: it defines the output row as the columns re-exposed by
// `spread` followed by the computed `fields`. Aliases, computed values,
// and nested materialisation (a field whose expression is a crossing) all
// live here. At least one of `spread` or `fields` must be present.
//
// Output attribute names must be unique. Every collision is rejected at
// bind time: field against field, field against a spread-produced column,
// and column against column across spread scopes. Bind an output `scope`
// when a later node references this projection's attributes by name.
type ProjectNode struct {
	// Computed output attributes, in order, after any spread.
	Fields []Field `json:"fields,omitempty"`
	// The id of the relation to project.
	Input string `json:"input"`
	Kind  string `json:"kind"`
	// Optional label bound to this projection's output row. Required only
	// when a later node refers to these attributes by name.
	//
	Scope *string `json:"scope,omitempty"`
	// Scopes whose columns are re-exposed in the output, in order, before
	// the computed `fields`. Names must not collide within or across
	// spreads or with a field.
	//
	Spread []string `json:"spread,omitempty"`
}

func (ProjectNode) isNode() {}

func (ProjectNode) NodeType() string { return "project" }

// Combine `left` and `right` into a relation whose rows carry the
// attributes of both sides, keeping pairs for which `on` is `TRUE` under
// three-valued logic.
//
//   - `inner` — only matched pairs.
//   - `left` — every left row at least once; the right attributes are NULL
//     when no right row matches.
//
// The two sides are independent relations: neither may reference a scope
// bound by the other (a dependent join is rejected at bind time), and `on`
// may not contain a crossing. Correlated nesting is expressed with a
// crossing in a projection field, not with a join. `semi` and `anti` are
// reserved and not accepted.
type JoinNode struct {
	// The join mode: `inner` keeps only matched pairs; `left` keeps every
	// left row, padding unmatched right attributes with NULL.
	//
	Join string `json:"join"`
	Kind string `json:"kind"`
	// The id of the left input relation.
	Left string `json:"left"`
	// The boolean join condition. Kept pairs are those for which it is
	// TRUE. Must not reference a crossing.
	//
	On Expr `json:"on"`
	// The id of the right input relation.
	Right string `json:"right"`
}

func (JoinNode) isNode() {}

func (JoinNode) NodeType() string { return "join" }

// Concatenate two or more input relations into one bag: the output
// contains every row of every input with its multiplicity preserved and
// performs no deduplication (SQL `UNION ALL`). A set union is the
// composition `distinct` over `concatenate`, under `distinct`'s
// canonical full-row identity.
//
// Inputs must have positionally compatible output shapes: the same number
// of columns and, at each position, the same column name and scalar kind.
// Nullability is widened positionally: an output column is nullable when
// the corresponding column of any input is nullable.
//
// The output carries those shared column names under `scope`, exactly as
// a scan exposes its columns under its scope. Input scopes do not remain
// visible above the concatenation.
//
// A concatenation imposes no logical order. Input declaration order and
// any ordering carried by an input have no observable effect on the
// output. An implementation may process or emit inputs in any order. The
// declaration order is structural only and has no relational or
// observable meaning. An observable ordered result requires an explicit
// `order` above the concatenation.
type ConcatenateNode struct {
	// The ids of the input relations. At least two; n-ary by design,
	// since concatenation is associative.
	//
	Inputs []string `json:"inputs"`
	Kind   string   `json:"kind"`
	// The label the output columns are bound to. Must be unique across
	// the query, under the same rules as a scan's scope.
	//
	Scope string `json:"scope"`
}

func (ConcatenateNode) isNode() {}

func (ConcatenateNode) NodeType() string { return "concatenate" }

// The bag intersection of `left` and `right` (SQL `INTERSECT`): the rows
// common to both inputs, matched by canonical full-row identity — the
// `distinct` operator's identity, where typed NULLs equal one another.
// With `quantifier: all`, a row occurring m times in `left` and n times
// in `right` occurs min(m, n) times in the output; with `quantifier:
// distinct`, each common row occurs exactly once.
//
// The inputs must be positionally compatible under `concatenate`'s rule:
// the same number of columns and, at each position, the same column name
// and scalar kind. Every output row is drawn from `left`, so the output
// columns carry `left`'s types and nullability, exposed under `scope`
// exactly as a scan exposes its columns. Input scopes are not visible
// above the node.
//
// The two sides are independent relations: neither may reference a scope
// bound by the other, exactly as a join's sides may not. The output has
// no inherent order; an observable ordered result requires an explicit
// `order` above it.
type IntersectNode struct {
	Kind string `json:"kind"`
	// The id of the left input relation.
	Left string `json:"left"`
	// The set quantifier: `all` keeps matched multiplicities (min of the
	// two sides); `distinct` emits each common row once.
	//
	Quantifier string `json:"quantifier"`
	// The id of the right input relation.
	Right string `json:"right"`
	// The label the output columns are bound to. Must be unique across
	// the query, under the same rules as a scan's scope.
	//
	Scope string `json:"scope"`
}

func (IntersectNode) isNode() {}

func (IntersectNode) NodeType() string { return "intersect" }

// The bag difference `left` minus `right` (SQL `EXCEPT`): the rows of
// `left` with `right`'s occurrences removed, matched by canonical
// full-row identity — the `distinct` operator's identity, where typed
// NULLs equal one another. With `quantifier: all`, a row occurring m
// times in `left` and n times in `right` occurs max(m − n, 0) times in
// the output; with `quantifier: distinct`, each distinct `left` row
// occurs exactly once when it does not occur in `right` at all.
//
// The inputs must be positionally compatible under `concatenate`'s rule:
// the same number of columns and, at each position, the same column name
// and scalar kind. Every output row is drawn from `left`, so the output
// columns carry `left`'s types and nullability, exposed under `scope`
// exactly as a scan exposes its columns. Input scopes are not visible
// above the node.
//
// The two sides are independent relations: neither may reference a scope
// bound by the other, exactly as a join's sides may not. The output has
// no inherent order; an observable ordered result requires an explicit
// `order` above it.
type ExceptNode struct {
	Kind string `json:"kind"`
	// The id of the left input relation.
	Left string `json:"left"`
	// The set quantifier: `all` subtracts occurrence counts; `distinct`
	// emits each distinct `left` row absent from `right` once.
	//
	Quantifier string `json:"quantifier"`
	// The id of the right input relation, whose occurrences are subtracted from `left`.
	Right string `json:"right"`
	// The label the output columns are bound to. Must be unique across
	// the query, under the same rules as a scan's scope.
	//
	Scope string `json:"scope"`
}

func (ExceptNode) isNode() {}

func (ExceptNode) NodeType() string { return "except" }

// Fold the rows of `input` into groups. With `groups`, it is GROUP BY: one
// output row per distinct combination of the group expressions. With no
// `groups`, it is the global fold: exactly one output row over the whole
// input, even when the input is empty. At least one of `groups` or `aggs`
// must be present.
//
// Over an empty input, a global `count` is `0` and every other fold
// (`sum`, `avg`, `min`, `max`) is NULL. The output row type is the group
// attributes followed by the aggregate term outputs; a node above an
// aggregate may reference only those. Bind an output `scope` when a later
// node references them by name.
type AggregateNode struct {
	// The aggregate folds computed for each group.
	Aggs []AggTerm `json:"aggs,omitempty"`
	// Grouping keys. One output row is produced per distinct combination
	// of their values. Omit for a single global fold.
	//
	Groups []GroupTerm `json:"groups,omitempty"`
	// The id of the relation to fold.
	Input string `json:"input"`
	Kind  string `json:"kind"`
	// Optional label bound to this aggregate's output row. Required only
	// when a later node refers to its groups or terms by name.
	//
	Scope *string `json:"scope,omitempty"`
}

func (AggregateNode) isNode() {}

func (AggregateNode) NodeType() string { return "aggregate" }

// Impose a logical ordering on `input` without changing its rows or row
// type. Rows sort by `terms` in order; within each term NULLs sort first
// ascending and last descending.
//
// When the output carries a known unique key (a scan's primary key, an
// aggregate's group columns), that key is appended as trailing ascending
// terms so tied rows order identically under every access path. Without a
// known unique key, the order of tied rows is unspecified. An explicit
// `order` is what makes a downstream `first` or `slice` a deterministic,
// path-independent selection.
type OrderNode struct {
	// The id of the relation to order.
	Input string `json:"input"`
	Kind  string `json:"kind"`
	// The ordering terms, applied in list order.
	Terms []OrderTerm `json:"terms"`
}

func (OrderNode) isNode() {}

func (OrderNode) NodeType() string { return "order" }

// Take a positional window of `input`: skip `offset` rows, then keep at
// most `limit`. `offset` defaults to 0. An omitted `limit` means
// unlimited; an explicit `0` keeps no rows.
//
// Slicing is positional. Over an ordered relation it selects a logical
// range; over an unordered one it is an arbitrary but deliberate selection
// (the `LIMIT` without `ORDER BY` contract), the one documented exception
// to path-independence.
type SliceNode struct {
	// The id of the relation to slice.
	Input string `json:"input"`
	Kind  string `json:"kind"`
	// Maximum rows to keep. Omitted means unlimited; an explicit `0` keeps
	// no rows.
	//
	Limit *int `json:"limit,omitempty"`
	// Rows to skip before taking. Defaults to 0.
	Offset *int `json:"offset,omitempty"`
}

func (SliceNode) isNode() {}

func (SliceNode) NodeType() string { return "slice" }

// One occurrence of a named binding: a fresh variable ranging over the
// binding's committed value, exactly as a scan is a fresh variable over
// a table's snapshot. The occurrence exposes the binding's output
// columns under its own `scope`; the binding's interior scopes
// are not visible through it. References are uniformly zero-or-many
// rows regardless of the body — determinism-sensitive consumers bring
// their own order or slice.
type RefNode struct {
	// The name of the binding this occurrence observes.
	Binding string `json:"binding"`
	Kind    string `json:"kind"`
	// The label this occurrence's rows are bound to. Must be unique
	// across the query, like a scan's scope.
	//
	Scope string `json:"scope"`
}

func (RefNode) isNode() {}

func (RefNode) NodeType() string { return "ref" }

// One occurrence of the enclosing recursive binding's frontier: the rows
// the previous iteration produced (the working table), exposed under a
// fresh `scope` exactly as `ref` exposes a committed binding. Legal only
// inside that binding's `step`, and exactly once. It denotes the frontier,
// never the accumulated result; the completed fixpoint value is observed
// from outside the binding through an ordinary `ref`.
type RecursiveRefNode struct {
	// The enclosing recursive binding whose frontier this observes.
	//
	Binding string `json:"binding"`
	Kind    string `json:"kind"`
	// The label this occurrence's rows are bound to. Must be unique across
	// the query, like a scan's scope.
	//
	Scope string `json:"scope"`
}

func (RecursiveRefNode) isNode() {}

func (RecursiveRefNode) NodeType() string { return "recursive_ref" }

// Remove duplicate complete rows from `input`. Two rows are duplicates when
// every corresponding datum has the same canonical row identity — the
// complete row, type- and order-significant, with typed NULL values equal to
// one another (deliberately unlike three-valued predicate equality). The
// output has `input`'s row type and contains at most one occurrence of each
// complete row. `distinct` fixes membership and multiplicity, not order: any
// observable ordering requires an explicit `order` above it, and an ordered
// input confers no ordering on the output.
type DistinctNode struct {
	// The id of the relation to deduplicate.
	Input string `json:"input"`
	Kind  string `json:"kind"`
}

func (DistinctNode) isNode() {}

func (DistinctNode) NodeType() string { return "distinct" }

// Selects the result relation and how its rows materialise into the
// response body:
//
//   - `many` — every row, as an array of objects.
//   - `first` — the first row as an object, or null when there are none.
//     Subject to the determinism rule: the root relation must be provably at
//     most one row or carry an explicit `order`.
//   - `exactly_one` — the single row as an object; it is an error if the
//     relation produces zero or more than one row.
//   - `scalar` — the sole column of the sole row as a bare value, or null
//     when there are no rows. The relation must have exactly one output
//     column and be statically at most one row. Ordering a multi-row
//     relation makes `first` deterministic but does not make it a valid
//     `scalar`.
type Root struct {
	// How the root relation's rows collapse into the response.
	Cardinality string `json:"cardinality"`
	// The id of the root relation node.
	Node string `json:"node"`
}

// A complete query: the relation forest plus its root selector.
//
// `nodes` is a flat map from caller-chosen id to relation node. Edges are
// the string ids a node names in its `input`, `inputs`, `left`, `right`,
// or a crossing's `node`. This encoding carries no sharing among ordinary
// nodes: an id names exactly one inline definition, referenced by exactly
// one consumer. The document is a finite forest — one tree rooted at
// `root.node` plus one tree per binding root — and cyclic, dangling,
// shared, or unreachable definitions are rejected before binding.
//
// `bindings` is the one deliberate sharing construct. A binding names
// and denotes a derived relation as a first-class relational value —
// the name is syntax, the denotation is semantics: one statement-local
// committed value, observed by every `ref` node under its own fresh
// scope, exactly as two scans of one table observe the same snapshot.
// Binding names, node ids, and scope labels are three separate
// namespaces.
type Query struct {
	// Named relational values, each identifying the root node of its
	// defining tree within `nodes`. A binding's tree is subject to the
	// same single-consumer rules as the main tree; `ref` nodes are edges
	// into this namespace, never ordinary node-consumer edges. Binding
	// trees may themselves reference other bindings: those references
	// form a separate acyclic dependency graph over the relation forest.
	// The single exception is a recursive binding, whose `step` references
	// the binding itself through `recursive_ref` — the one sanctioned
	// cycle. Every declared binding must be referenced at least once.
	//
	Bindings map[string]Binding `json:"bindings,omitempty"`
	// The relation nodes of the query, keyed by caller-chosen id. Ids are
	// opaque labels with no meaning beyond linking a consumer to its
	// input; they carry no sharing or materialisation identity.
	//
	Nodes map[string]Node `json:"nodes"`
	Root  Root            `json:"root"`
}
