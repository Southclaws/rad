package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

type GroupTerm struct {
	Name string
	Slot lir.SlotID
	Expr Expr
}

type AggTerm struct {
	Fn   lir.AggFn
	Arg  Expr
	Name string
	Slot lir.SlotID
	T    lir.Type
}

// Count is non-null int64; average is nullable float64; every other fold keeps
// the argument kind and becomes nullable because an empty input has no value.
func AggTermType(fn lir.AggFn, arg Expr) lir.Type {
	switch fn {
	case lir.AggCount:
		return lir.Type{Kind: lir.KindInt64}
	case lir.AggAvg:
		return lir.Type{Kind: lir.KindFloat64, Nullable: true}
	default:
		t := arg.Type()
		t.Nullable = true
		return t
	}
}

// Aggregate produces one row per group, or one row for a global fold.
type Aggregate struct {
	laws
	In     Relation
	Groups []GroupTerm
	Terms  []AggTerm
}

func NewAggregate(in Relation, groups []GroupTerm, terms []AggTerm) *Aggregate {
	fields := make([]lir.Field, 0, len(groups)+len(terms))
	slots := make([]lir.SlotID, 0, len(groups)+len(terms))
	free := in.FreeSlots()
	for _, group := range groups {
		fields = append(fields, lir.Field{Name: group.Name, Slot: group.Slot, Type: group.Expr.Type()})
		slots = append(slots, group.Slot)
		free = free.Union(group.Expr.FreeSlots().Without(in.Produced()))
	}
	for _, term := range terms {
		fields = append(fields, lir.Field{Name: term.Name, Slot: term.Slot, Type: term.T})
		slots = append(slots, term.Slot)
		if term.Arg != nil {
			free = free.Union(term.Arg.FreeSlots().Without(in.Produced()))
		}
	}
	card := lir.Cardinality{Min: 1, Max: 1}
	if len(groups) > 0 {
		card = lir.Cardinality{Min: 0, Max: in.Card().Max}
	}
	return &Aggregate{
		laws: laws{
			out:      lir.RowType{Fields: fields},
			free:     free,
			produced: in.Produced().Union(NewSlotSet(slots...)),
			card:     card,
		},
		In: in, Groups: groups, Terms: terms,
	}
}

func (a *Aggregate) Inputs() []Relation { return []Relation{a.In} }
