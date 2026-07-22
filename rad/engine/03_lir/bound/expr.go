package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

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

// BranchArm pairs a boolean predicate with the result selected when it is
// the first predicate to evaluate to TRUE.
type BranchArm struct {
	When, Then Expr
}

// Branch is ordered, lazy branching. The binder guarantees every arm result
// and Else share one scalar kind and that the whole subtree is crossing-free,
// so evaluation is a pure in-order predicate walk that touches nothing beyond
// the selected result.
type Branch struct {
	Arms []BranchArm
	Else Expr
	t    lir.Type
}

// NewBranch takes the result kind from Else; nullability is the union over
// every arm result and Else, since any of them may be the one produced.
func NewBranch(arms []BranchArm, els Expr) Branch {
	t := els.Type()
	for _, a := range arms {
		t.Nullable = t.Nullable || a.Then.Type().Nullable
	}
	return Branch{Arms: arms, Else: els, t: t}
}

func (b Branch) Type() lir.Type { return b.t }
func (b Branch) FreeSlots() SlotSet {
	s := b.Else.FreeSlots()
	for _, a := range b.Arms {
		s = s.Union(a.When.FreeSlots()).Union(a.Then.FreeSlots())
	}
	return s
}

// TextMatch tests Value against a compiled anchored Pattern. The binder
// guarantees Value is text and compiles the constant parts into Pattern once.
type TextMatch struct {
	Value    Expr
	Pattern  TextPattern
	nullable bool
}

// NewTextMatch builds the bound match. The result is boolean, nullable exactly
// when Value is: a non-null value always matches to a total TRUE/FALSE, so
// NULL is the only source of UNKNOWN.
func NewTextMatch(value Expr, pattern TextPattern) TextMatch {
	return TextMatch{Value: value, Pattern: pattern, nullable: value.Type().Nullable}
}

func (m TextMatch) Type() lir.Type     { return lir.Type{Kind: lir.KindBool, Nullable: m.nullable} }
func (m TextMatch) FreeSlots() SlotSet { return m.Value.FreeSlots() }
