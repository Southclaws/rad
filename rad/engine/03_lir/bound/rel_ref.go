package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

func referenceLaws(fields []lir.Field) laws {
	out := lir.RowType{Fields: fields}
	return laws{
		out:      out,
		produced: NewSlotSet(out.Slots()...),
		card:     lir.Cardinality{Min: 0, Max: lir.Unbounded},
	}
}

// Ref is one scoped occurrence of a committed binding value. Canon maps each
// occurrence slot to the corresponding canonical binding slot.
type Ref struct {
	laws
	Binding string
	Scope   string
	Canon   []lir.SlotID
}

func NewRef(binding, scope string, fields []lir.Field, canon []lir.SlotID) *Ref {
	return &Ref{
		laws:    referenceLaws(fields),
		Binding: binding,
		Scope:   scope,
		Canon:   canon,
	}
}

func (r *Ref) Inputs() []Relation { return nil }

// RecursiveRef exposes the previous recursive iteration under fresh occurrence
// slots. Canon maps occurrence slots to frontier slots.
type RecursiveRef struct {
	laws
	Binding string
	Scope   string
	Canon   []lir.SlotID
}

func NewRecursiveRef(binding, scope string, fields []lir.Field, canon []lir.SlotID) *RecursiveRef {
	return &RecursiveRef{
		laws:    referenceLaws(fields),
		Binding: binding,
		Scope:   scope,
		Canon:   canon,
	}
}

func (r *RecursiveRef) Inputs() []Relation { return nil }

// Binding is a named relational value. Recursive bindings evaluate Root as the
// anchor and Step against successive frontiers until accumulation stabilizes.
type Binding struct {
	Name          string
	Root          Relation
	Out           lir.RowType
	PlanSensitive bool
	Recursive     bool
	Step          Relation
	Accumulation  lir.RecursiveAccumulationMode
}
