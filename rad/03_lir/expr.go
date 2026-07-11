package lir

// Unbound expressions: names and raw literals, exactly as a frontend
// produces them. The binder resolves columns to slots, coerces literals
// against the column types they meet, and infers every type. There is
// deliberately no special equality node — eq is an ordinary Binary op, and
// the planner's access-path analysis extracts searchable constraints from
// the regular tree rather than the tree being shaped for the optimizer.

// Expr is the sealed unbound expression interface.
type Expr interface{ expr() }

// Literal carries a raw wire scalar — string, json.Number, bool, or nil.
// It is typed by the binder in context (a JSON number becomes int64 or
// float64 by the column it meets, never by guessing; nil adopts the
// column's type as NULL).
type Literal struct{ Raw any }

// Column names a column of a bound scope. Scope is required: bare column
// references stop working the moment two relations are in play, so
// frontends always qualify.
type Column struct{ Scope, Name string }

// UnaryOp enumerates the unary operators.
type UnaryOp string

const (
	OpNot       UnaryOp = "not"
	OpNegate    UnaryOp = "negate"
	OpIsNull    UnaryOp = "is_null"
	OpIsNotNull UnaryOp = "is_not_null"
)

// Unary applies Op to X.
type Unary struct {
	Op UnaryOp
	X  Expr
}

// BinaryOp enumerates the binary operators.
type BinaryOp string

const (
	OpEq  BinaryOp = "eq"
	OpNe  BinaryOp = "ne"
	OpLt  BinaryOp = "lt"
	OpLte BinaryOp = "lte"
	OpGt  BinaryOp = "gt"
	OpGte BinaryOp = "gte"
	OpAnd BinaryOp = "and"
	OpOr  BinaryOp = "or"
	OpAdd BinaryOp = "add"
	OpSub BinaryOp = "sub"
	OpMul BinaryOp = "mul"
	OpDiv BinaryOp = "div"
)

// Binary applies Op to L and R. And/or are binary; frontends left-fold
// longer chains.
type Binary struct {
	Op   BinaryOp
	L, R Expr
}

// Call invokes a named function. The registry is empty this arc; the node
// exists so the grammar doesn't need to change when functions arrive.
type Call struct {
	Fn   string
	Args []Expr
}

// Cast converts X to a scalar kind.
type Cast struct {
	X  Expr
	To Kind
}

// ── cardinality crossings ───────────────────────────────────────────────────
//
// The only doors from Relation into Expr. Each states how many-becomes-one;
// a relation can appear almost anywhere, but never *as* a scalar without
// declaring the conversion, and never with plan-dependent contents: the
// binder requires First's relation to be statically at-most-one or
// explicitly ordered, and Scalar's to be single-column and at-most-one.

// Exists is true iff Rel has at least one row. Never NULL.
type Exists struct{ Rel Relation }

// First materialises Rel's row as a nested object, or NULL when empty.
type First struct{ Rel Relation }

// Scalar asserts Rel has one output column and at most one row, and yields
// that value — NULL when there is no row.
type Scalar struct{ Rel Relation }

// Array materialises all of Rel's rows as a nested array; empty, never NULL.
type Array struct{ Rel Relation }

func (Literal) expr() {}
func (Column) expr()  {}
func (Unary) expr()   {}
func (Binary) expr()  {}
func (Call) expr()    {}
func (Cast) expr()    {}
func (Exists) expr()  {}
func (First) expr()   {}
func (Scalar) expr()  {}
func (Array) expr()   {}
