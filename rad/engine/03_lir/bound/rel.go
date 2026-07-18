package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

// Relation exposes the laws derived by binding a relational expression.
type Relation interface {
	Output() lir.RowType
	Inputs() []Relation
	FreeSlots() SlotSet
	Produced() SlotSet
	Card() lir.Cardinality
	Ordered() bool
}

type laws struct {
	out      lir.RowType
	free     SlotSet
	produced SlotSet
	card     lir.Cardinality
	ordered  bool
}

func (l *laws) Output() lir.RowType   { return l.out }
func (l *laws) FreeSlots() SlotSet    { return l.free }
func (l *laws) Produced() SlotSet     { return l.produced }
func (l *laws) Card() lir.Cardinality { return l.card }
func (l *laws) Ordered() bool         { return l.ordered }

func Ordered(rel Relation) bool { return rel.Ordered() }

// RefineCard tightens cardinality bounds using facts unavailable to a node's
// constructor, such as equality constraints covering a unique key.
func (l *laws) RefineCard(c lir.Cardinality) {
	if c.Min > l.card.Min {
		l.card.Min = c.Min
	}
	if c.Max != lir.Unbounded && (l.card.Max == lir.Unbounded || c.Max < l.card.Max) {
		l.card.Max = c.Max
	}
}
