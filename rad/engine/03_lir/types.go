package lir

import (
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

// SlotID names one output attribute of a bound relation. The binder assigns
// slots densely per query; every bound column reference is a slot reference.
// Slots are what make the IR relationally closed: a Project or Aggregate
// output is addressable by later operators exactly like a scanned column.
// The zero value marks "unassigned" (unbound IR carries names, not slots).
type SlotID int32

// Kind classifies a static type. The four scalar kinds mirror catalog.Type;
// Row and Array exist for nested values produced by cardinality crossings.
type Kind string

const (
	KindText    Kind = Kind(catalog.TypeText)
	KindInt64   Kind = Kind(catalog.TypeInt64)
	KindFloat64 Kind = Kind(catalog.TypeFloat64)
	KindBool    Kind = Kind(catalog.TypeBool)
	KindRow     Kind = "row"
	KindArray   Kind = "array"
)

// KindOf lifts a catalog column type into a type kind.
func KindOf(t catalog.Type) Kind { return Kind(t) }

// Scalar reports whether k is one of the four storable scalar kinds.
func (k Kind) Scalar() bool {
	switch k {
	case KindText, KindInt64, KindFloat64, KindBool:
		return true
	}
	return false
}

// CatalogType maps a scalar kind back to its catalog type. It is only
// meaningful when k.Scalar() is true.
func (k Kind) CatalogType() catalog.Type { return catalog.Type(k) }

// Numeric reports whether k participates in arithmetic.
func (k Kind) Numeric() bool { return k == KindInt64 || k == KindFloat64 }

// Type is the static type of an expression or relation attribute, including
// nullability. Row and Elem are set only for the nested kinds.
type Type struct {
	Kind     Kind
	Nullable bool
	Row      *RowType // KindRow: the nested row's shape
	Elem     *Type    // KindArray: the element type
}

func (t Type) String() string {
	s := string(t.Kind)
	if t.Kind == KindArray && t.Elem != nil {
		s = fmt.Sprintf("array<%s>", t.Elem)
	}
	if t.Nullable {
		return s + "?"
	}
	return s
}

// ScalarType builds a scalar Type from a catalog column type.
func ScalarType(t catalog.Type, nullable bool) Type {
	return Type{Kind: KindOf(t), Nullable: nullable}
}

// Field is one attribute of a relation's output row: a name for humans and
// the wire, a slot for the bound IR, and a static type.
type Field struct {
	Name string
	Slot SlotID
	Type Type
}

// RowType is the ordered shape of a relation's output.
type RowType struct {
	Fields []Field
}

// Lookup finds a field by name.
func (r RowType) Lookup(name string) (Field, bool) {
	for _, f := range r.Fields {
		if f.Name == name {
			return f, true
		}
	}
	return Field{}, false
}

// Slots lists the row's slot IDs in field order.
func (r RowType) Slots() []SlotID {
	out := make([]SlotID, len(r.Fields))
	for i, f := range r.Fields {
		out[i] = f.Slot
	}
	return out
}

// Unbounded marks a cardinality with no upper bound.
const Unbounded = -1

// Cardinality bounds how many rows a relation can produce. Max is a row
// count or Unbounded.
type Cardinality struct {
	Min, Max int
}

// AtMostOne reports whether the relation statically produces 0 or 1 rows —
// the condition that lets First and Scalar cross deterministically without
// an explicit ordering.
func (c Cardinality) AtMostOne() bool { return c.Max != Unbounded && c.Max <= 1 }

func (c Cardinality) String() string {
	if c.Max == Unbounded {
		return fmt.Sprintf("%d..many", c.Min)
	}
	return fmt.Sprintf("%d..%d", c.Min, c.Max)
}
