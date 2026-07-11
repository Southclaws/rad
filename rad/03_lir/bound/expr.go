package bound

import (
	"fmt"

	lir "rad/rad/03_lir"
)

// Expr is the bound expression law: a precomputed static type and the slots
// it references.
type Expr interface {
	Type() lir.Type
	FreeSlots() SlotSet
}

// Literal is a typed constant. The binder coerced the wire's raw scalar
// against the column type it met.
type Literal struct {
	V lir.Value
}

func (l Literal) Type() lir.Type {
	return lir.ScalarType(l.V.Type, l.V.Null)
}
func (l Literal) FreeSlots() SlotSet { return SlotSet{} }

// SlotRef reads one attribute of a relation output. Name is diagnostic only;
// the slot is the identity.
type SlotRef struct {
	Slot lir.SlotID
	Name string
	T    lir.Type
}

func (s SlotRef) Type() lir.Type     { return s.T }
func (s SlotRef) FreeSlots() SlotSet { return NewSlotSet(s.Slot) }

// Unary applies a unary operator.
type Unary struct {
	Op lir.UnaryOp
	X  Expr
	t  lir.Type
}

func NewUnary(op lir.UnaryOp, x Expr) Unary {
	var t lir.Type
	switch op {
	case lir.OpNot:
		t = lir.Type{Kind: lir.KindBool, Nullable: x.Type().Nullable}
	case lir.OpNegate:
		t = x.Type()
	case lir.OpIsNull, lir.OpIsNotNull:
		t = lir.Type{Kind: lir.KindBool}
	}
	return Unary{Op: op, X: x, t: t}
}

func (u Unary) Type() lir.Type     { return u.t }
func (u Unary) FreeSlots() SlotSet { return u.X.FreeSlots() }

// Binary applies a binary operator. Comparisons yield Bool, nullable when
// either operand is (a NULL operand makes the comparison UNKNOWN);
// arithmetic promotes int64 to float64 when either side is float.
type Binary struct {
	Op   lir.BinaryOp
	L, R Expr
	t    lir.Type
}

func NewBinary(op lir.BinaryOp, l, r Expr) Binary {
	nullable := l.Type().Nullable || r.Type().Nullable
	var t lir.Type
	switch op {
	case lir.OpEq, lir.OpNe, lir.OpLt, lir.OpLte, lir.OpGt, lir.OpGte,
		lir.OpAnd, lir.OpOr:
		t = lir.Type{Kind: lir.KindBool, Nullable: nullable}
	case lir.OpAdd, lir.OpSub, lir.OpMul, lir.OpDiv:
		kind := lir.KindInt64
		if l.Type().Kind == lir.KindFloat64 || r.Type().Kind == lir.KindFloat64 {
			kind = lir.KindFloat64
		}
		t = lir.Type{Kind: kind, Nullable: nullable}
	}
	return Binary{Op: op, L: l, R: r, t: t}
}

func (b Binary) Type() lir.Type     { return b.t }
func (b Binary) FreeSlots() SlotSet { return b.L.FreeSlots().Union(b.R.FreeSlots()) }

// Call invokes a named function. The registry is empty this arc; the binder
// rejects every call, and the node exists so the grammar is stable.
type Call struct {
	Fn   string
	Args []Expr
	T    lir.Type
}

func (c Call) Type() lir.Type { return c.T }
func (c Call) FreeSlots() SlotSet {
	var s SlotSet
	for _, a := range c.Args {
		s = s.Union(a.FreeSlots())
	}
	return s
}

// Cast converts to a scalar kind.
type Cast struct {
	X  Expr
	To lir.Kind
	t  lir.Type
}

func NewCast(x Expr, to lir.Kind) Cast {
	return Cast{X: x, To: to, t: lir.Type{Kind: to, Nullable: x.Type().Nullable}}
}

func (c Cast) Type() lir.Type     { return c.t }
func (c Cast) FreeSlots() SlotSet { return c.X.FreeSlots() }

// ── cardinality crossings ───────────────────────────────────────────────────
//
// A crossing's free slots are its relation's free slots: whatever the
// sub-relation produces internally stays internal, and whatever it needs
// from enclosing scopes is exactly what makes the crossing correlated.

// Exists is true iff Rel has at least one row. Never NULL.
type Exists struct {
	Rel Relation
}

func (e Exists) Type() lir.Type     { return lir.Type{Kind: lir.KindBool} }
func (e Exists) FreeSlots() SlotSet { return e.Rel.FreeSlots() }

// First materialises Rel's row as a nested object, or NULL when empty. The
// binder guarantees determinism: Rel is statically at-most-one or explicitly
// ordered.
type First struct {
	Rel Relation
	t   lir.Type
}

func NewFirst(rel Relation) First {
	out := rel.Output()
	return First{Rel: rel, t: lir.Type{Kind: lir.KindRow, Nullable: true, Row: &out}}
}

func (f First) Type() lir.Type     { return f.t }
func (f First) FreeSlots() SlotSet { return f.Rel.FreeSlots() }

// Scalar asserts Rel has one output column and statically at most one row,
// and yields that value — NULL when there is no row.
type Scalar struct {
	Rel Relation
	t   lir.Type
}

func NewScalar(rel Relation) (Scalar, error) {
	fields := rel.Output().Fields
	if len(fields) != 1 {
		return Scalar{}, fmt.Errorf("planner: scalar crossing needs a single-column relation, got %d columns", len(fields))
	}
	t := fields[0].Type
	t.Nullable = true
	return Scalar{Rel: rel, t: t}, nil
}

func (s Scalar) Type() lir.Type     { return s.t }
func (s Scalar) FreeSlots() SlotSet { return s.Rel.FreeSlots() }

// Array materialises all of Rel's rows as a nested array; empty, never NULL.
type Array struct {
	Rel Relation
	t   lir.Type
}

func NewArray(rel Relation) Array {
	out := rel.Output()
	elem := lir.Type{Kind: lir.KindRow, Row: &out}
	return Array{Rel: rel, t: lir.Type{Kind: lir.KindArray, Elem: &elem}}
}

func (a Array) Type() lir.Type     { return a.t }
func (a Array) FreeSlots() SlotSet { return a.Rel.FreeSlots() }
