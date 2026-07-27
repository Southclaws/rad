package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

type Filter struct {
	laws
	In   Relation
	Pred Expr
}

func NewFilter(in Relation, pred Expr) *Filter {
	return &Filter{
		laws: laws{
			out:      in.Output(),
			free:     in.FreeSlots().Union(pred.FreeSlots().Without(in.Produced())),
			produced: in.Produced(),
			card:     lir.Cardinality{Min: 0, Max: in.Card().Max},
			ordered:  in.Ordered(),
		},
		In: in, Pred: pred,
	}
}

func (f *Filter) Inputs() []Relation { return []Relation{f.In} }

type ProjField struct {
	Name string
	Slot lir.SlotID
	Expr Expr
}

// Project establishes a new row type while preserving input order.
type Project struct {
	laws
	In     Relation
	Fields []ProjField
	Scope  string
}

func NewProject(in Relation, scope string, fields []ProjField) *Project {
	out := make([]lir.Field, len(fields))
	slots := make([]lir.SlotID, len(fields))
	free := in.FreeSlots()
	for i, field := range fields {
		out[i] = lir.Field{Name: field.Name, Slot: field.Slot, Type: field.Expr.Type()}
		slots[i] = field.Slot
		free = free.Union(field.Expr.FreeSlots().Without(in.Produced()))
	}
	return &Project{
		laws: laws{
			out:      lir.RowType{Fields: out},
			free:     free,
			produced: in.Produced().Union(NewSlotSet(slots...)),
			card:     in.Card(),
			ordered:  in.Ordered(),
		},
		In: in, Fields: fields, Scope: scope,
	}
}

func (p *Project) Inputs() []Relation { return []Relation{p.In} }

type OrderTerm struct {
	Expr Expr
	Desc bool
}

type Order struct {
	laws
	In    Relation
	Terms []OrderTerm
}

func NewOrder(in Relation, terms []OrderTerm) *Order {
	free := in.FreeSlots()
	for _, term := range terms {
		free = free.Union(term.Expr.FreeSlots().Without(in.Produced()))
	}
	return &Order{
		laws: laws{
			out:      in.Output(),
			free:     free,
			produced: in.Produced(),
			card:     in.Card(),
			ordered:  true,
		},
		In: in, Terms: terms,
	}
}

func (o *Order) Inputs() []Relation { return []Relation{o.In} }

type Slice struct {
	laws
	In     Relation
	Offset int
	Limit  *int
}

func NewSlice(in Relation, offset int, limit *int) *Slice {
	card := in.Card()
	card.Min = 0
	if limit != nil && (card.Max == lir.Unbounded || *limit < card.Max) {
		card.Max = *limit
	}
	return &Slice{
		laws: laws{
			out:      in.Output(),
			free:     in.FreeSlots(),
			produced: in.Produced(),
			card:     card,
			ordered:  in.Ordered(),
		},
		In: in, Offset: offset, Limit: limit,
	}
}

func (s *Slice) Inputs() []Relation { return []Relation{s.In} }

// Distinct removes duplicate complete rows and discards input ordering.
type Distinct struct {
	laws
	In Relation
}

func NewDistinct(in Relation) *Distinct {
	return &Distinct{
		laws: laws{
			out:      in.Output(),
			free:     in.FreeSlots(),
			produced: in.Produced(),
			card:     lir.Cardinality{Min: 0, Max: in.Card().Max},
		},
		In: in,
	}
}

func (d *Distinct) Inputs() []Relation { return []Relation{d.In} }
