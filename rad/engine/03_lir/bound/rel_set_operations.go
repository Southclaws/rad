package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

// The binary set operations: bag intersection and bag difference by
// canonical full-row identity, each under an ALL/DISTINCT quantifier. Every
// output row is drawn from the left input, so the output row type carries
// the left side's types and nullability on fresh slots (allocated by the
// binder), remapped positionally at execution. Output order is not defined.

// Intersect keeps rows common to both sides: min of the two multiplicities
// under QuantifierAll, one occurrence per common row under
// QuantifierDistinct.
type Intersect struct {
	laws
	L, R       Relation
	Quantifier lir.SetQuantifier
	Scope      string
}

func NewIntersect(left, right Relation, quantifier lir.SetQuantifier, scope string, fields []lir.Field) *Intersect {
	upper := left.Card().Max
	if r := right.Card().Max; upper == lir.Unbounded || (r != lir.Unbounded && r < upper) {
		upper = r
	}
	return &Intersect{
		laws:       setOperationLaws(left, right, fields, lir.Cardinality{Min: 0, Max: upper}),
		L:          left,
		R:          right,
		Quantifier: quantifier,
		Scope:      scope,
	}
}

func (i *Intersect) Inputs() []Relation { return []Relation{i.L, i.R} }

// Except keeps the left rows that survive subtracting the right side's
// occurrences: max(m−n, 0) under QuantifierAll, one occurrence per distinct
// left row absent from the right under QuantifierDistinct.
type Except struct {
	laws
	L, R       Relation
	Quantifier lir.SetQuantifier
	Scope      string
}

func NewExcept(left, right Relation, quantifier lir.SetQuantifier, scope string, fields []lir.Field) *Except {
	return &Except{
		laws:       setOperationLaws(left, right, fields, lir.Cardinality{Min: 0, Max: left.Card().Max}),
		L:          left,
		R:          right,
		Quantifier: quantifier,
		Scope:      scope,
	}
}

func (e *Except) Inputs() []Relation { return []Relation{e.L, e.R} }

func setOperationLaws(left, right Relation, fields []lir.Field, card lir.Cardinality) laws {
	out := lir.RowType{Fields: fields}
	produced := left.Produced().Union(right.Produced())
	return laws{
		out:      out,
		free:     left.FreeSlots().Union(right.FreeSlots()),
		produced: produced.Union(NewSlotSet(out.Slots()...)),
		card:     card,
	}
}
