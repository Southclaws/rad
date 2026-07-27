package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

// Concatenate is the n-ary bag concatenation. Its output row type is fresh:
// the binder allocates a new slot per column (names shared by every input,
// nullability OR'd across them), and each input's rows are remapped onto
// those slots positionally at execution. Output order is not defined.
type Concatenate struct {
	laws
	Ins   []Relation
	Scope string
}

// NewConcatenate builds the bound concatenation over inputs the binder has
// already verified positionally compatible. fields is the fresh output row
// type.
func NewConcatenate(ins []Relation, scope string, fields []lir.Field) *Concatenate {
	out := lir.RowType{Fields: fields}
	var free, produced SlotSet
	card := lir.Cardinality{}
	for _, in := range ins {
		free = free.Union(in.FreeSlots())
		produced = produced.Union(in.Produced())
		c := in.Card()
		card.Min += c.Min
		if card.Max == lir.Unbounded || c.Max == lir.Unbounded {
			card.Max = lir.Unbounded
		} else {
			card.Max += c.Max
		}
	}
	return &Concatenate{
		laws: laws{
			out:      out,
			free:     free,
			produced: produced.Union(NewSlotSet(out.Slots()...)),
			card:     card,
		},
		Ins: ins, Scope: scope,
	}
}

func (c *Concatenate) Inputs() []Relation { return c.Ins }
