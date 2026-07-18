package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

// Query is a bound root and its dependency-ordered bindings. Slots is the
// first unallocated slot available to planning.
type Query struct {
	Root     Relation
	Card     lir.RootCard
	Bindings []*Binding
	Slots    lir.SlotID
}
