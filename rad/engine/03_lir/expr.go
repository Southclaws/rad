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
// A non-nil Raw is typed by the binder in context (a JSON number becomes
// int64 or float64 by the column it meets, never by guessing). A nil Raw is
// a NULL; Kind, when set, is its declared scalar type, so a NULL with no
// surrounding context — a projected NULL — still binds. An unset Kind keeps
// the context-typed behaviour, and a contextless bare NULL is rejected.
type Literal struct {
	Raw  any
	Kind Kind
}

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

// Cast converts X to a scalar kind.
type Cast struct {
	X  Expr
	To Kind
}

// BranchArm is one arm of a Branch: a boolean predicate and the result
// selected when it is the first predicate to evaluate to TRUE.
type BranchArm struct {
	When, Then Expr
}

// Branch is ordered, lazy branching: arms are tested in order, the first
// TRUE predicate selects its result, FALSE and UNKNOWN predicates fall
// through, and Else is the result when no arm matches. Unselected result
// expressions are never evaluated — the laziness is contract, not
// optimization, so an error in a never-selected arm is unobservable.
type Branch struct {
	Arms []BranchArm
	Else Expr
}

// TextMatchPart is one element of a text_match pattern: a literal span or a
// wildcard. The pattern is a bind-time constant, so parts carry data, never
// expressions.
type TextMatchPart interface{ textMatchPart() }

// LiteralPart is a literal span matched verbatim (under the engine's ordinary
// text equality). Never empty.
type LiteralPart struct{ Value string }

// AnyManyPart matches zero or more characters (SQL `%`).
type AnyManyPart struct{}

func (LiteralPart) textMatchPart() {}
func (AnyManyPart) textMatchPart() {}

// TextMatch tests Value against an anchored pattern of Parts. It answers only
// whether the text matches; literal spans compare under the engine's ordinary
// text equality, with no case/collation knob of its own. Value is the only
// per-row operand; a NULL Value makes the result UNKNOWN.
type TextMatch struct {
	Value Expr
	Parts []TextMatchPart
}

func (Literal) expr()   {}
func (Column) expr()    {}
func (Unary) expr()     {}
func (Binary) expr()    {}
func (Cast) expr()      {}
func (Branch) expr()    {}
func (TextMatch) expr() {}
