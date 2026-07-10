package bound

import (
	"fmt"

	qir "rad/rad/03_qir"
)

// Expr is the bound expression law: a precomputed static type and the slots
// it references.
type Expr interface {
	Type() qir.Type
	FreeSlots() SlotSet
}

// Literal is a typed constant. The binder coerced the wire's raw scalar
// against the column type it met.
type Literal struct {
	V qir.Value
}

func (l Literal) Type() qir.Type {
	return qir.ScalarType(l.V.Type, l.V.Null)
}
func (l Literal) FreeSlots() SlotSet { return SlotSet{} }

// SlotRef reads one attribute of a relation output. Name is diagnostic only;
// the slot is the identity.
type SlotRef struct {
	Slot qir.SlotID
	Name string
	T    qir.Type
}

func (s SlotRef) Type() qir.Type      { return s.T }
func (s SlotRef) FreeSlots() SlotSet  { return NewSlotSet(s.Slot) }

// Unary applies a unary operator.
type Unary struct {
	Op qir.UnaryOp
	X  Expr
	t  qir.Type
}

func NewUnary(op qir.UnaryOp, x Expr) Unary {
	var t qir.Type
	switch op {
	case qir.OpNot:
		t = qir.Type{Kind: qir.KindBool, Nullable: x.Type().Nullable}
	case qir.OpNegate:
		t = x.Type()
	case qir.OpIsNull, qir.OpIsNotNull:
		t = qir.Type{Kind: qir.KindBool}
	}
	return Unary{Op: op, X: x, t: t}
}

func (u Unary) Type() qir.Type     { return u.t }
func (u Unary) FreeSlots() SlotSet { return u.X.FreeSlots() }

// Binary applies a binary operator. Comparisons yield Bool, nullable when
// either operand is (a NULL operand makes the comparison UNKNOWN);
// arithmetic promotes int64 to float64 when either side is float.
type Binary struct {
	Op   qir.BinaryOp
	L, R Expr
	t    qir.Type
}

func NewBinary(op qir.BinaryOp, l, r Expr) Binary {
	nullable := l.Type().Nullable || r.Type().Nullable
	var t qir.Type
	switch op {
	case qir.OpEq, qir.OpNe, qir.OpLt, qir.OpLte, qir.OpGt, qir.OpGte,
		qir.OpAnd, qir.OpOr:
		t = qir.Type{Kind: qir.KindBool, Nullable: nullable}
	case qir.OpAdd, qir.OpSub, qir.OpMul, qir.OpDiv:
		kind := qir.KindInt64
		if l.Type().Kind == qir.KindFloat64 || r.Type().Kind == qir.KindFloat64 {
			kind = qir.KindFloat64
		}
		t = qir.Type{Kind: kind, Nullable: nullable}
	}
	return Binary{Op: op, L: l, R: r, t: t}
}

func (b Binary) Type() qir.Type     { return b.t }
func (b Binary) FreeSlots() SlotSet { return b.L.FreeSlots().Union(b.R.FreeSlots()) }

// Call invokes a named function. The registry is empty this arc; the binder
// rejects every call, and the node exists so the grammar is stable.
type Call struct {
	Fn   string
	Args []Expr
	T    qir.Type
}

func (c Call) Type() qir.Type { return c.T }
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
	To qir.Kind
	t  qir.Type
}

func NewCast(x Expr, to qir.Kind) Cast {
	return Cast{X: x, To: to, t: qir.Type{Kind: to, Nullable: x.Type().Nullable}}
}

func (c Cast) Type() qir.Type     { return c.t }
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

func (e Exists) Type() qir.Type     { return qir.Type{Kind: qir.KindBool} }
func (e Exists) FreeSlots() SlotSet { return e.Rel.FreeSlots() }

// First materialises Rel's row as a nested object, or NULL when empty. The
// binder guarantees determinism: Rel is statically at-most-one or explicitly
// ordered.
type First struct {
	Rel Relation
	t   qir.Type
}

func NewFirst(rel Relation) First {
	out := rel.Output()
	return First{Rel: rel, t: qir.Type{Kind: qir.KindRow, Nullable: true, Row: &out}}
}

func (f First) Type() qir.Type     { return f.t }
func (f First) FreeSlots() SlotSet { return f.Rel.FreeSlots() }

// Scalar asserts Rel has one output column and statically at most one row,
// and yields that value — NULL when there is no row.
type Scalar struct {
	Rel Relation
	t   qir.Type
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

func (s Scalar) Type() qir.Type     { return s.t }
func (s Scalar) FreeSlots() SlotSet { return s.Rel.FreeSlots() }

// Array materialises all of Rel's rows as a nested array; empty, never NULL.
type Array struct {
	Rel Relation
	t   qir.Type
}

func NewArray(rel Relation) Array {
	out := rel.Output()
	elem := qir.Type{Kind: qir.KindRow, Row: &out}
	return Array{Rel: rel, t: qir.Type{Kind: qir.KindArray, Elem: &elem}}
}

func (a Array) Type() qir.Type     { return a.t }
func (a Array) FreeSlots() SlotSet { return a.Rel.FreeSlots() }
