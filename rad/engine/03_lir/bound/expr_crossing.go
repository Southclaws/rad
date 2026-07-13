package bound

import (
	"github.com/Southclaws/rad/rad/engine/reject"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// A crossing's free slots are its relation's free slots: whatever the
// sub-relation produces internally stays internal, and whatever it needs from
// enclosing scopes is exactly what makes the crossing correlated.

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
		return Scalar{}, reject.Inputf("planner: scalar crossing needs a single-column relation, got %d columns", len(fields))
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
