package lir

// The cardinality crossings are the only doors from Relation into Expr. Each
// states how many-becomes-one; a relation can appear almost anywhere, but never
// as a scalar without declaring the conversion, or with plan-dependent
// contents. The binder requires First's relation to be statically at-most-one
// or explicitly ordered, and Scalar's to be single-column and at-most-one.

// Exists is true iff Rel has at least one row. Never NULL.
type Exists struct{ Rel Relation }

// First materialises Rel's row as a nested object, or NULL when empty.
type First struct{ Rel Relation }

// Scalar asserts Rel has one output column and at most one row, and yields
// that value — NULL when there is no row.
type Scalar struct{ Rel Relation }

// Array materialises all of Rel's rows as a nested array; empty, never NULL.
type Array struct{ Rel Relation }

func (Exists) expr() {}
func (First) expr()  {}
func (Scalar) expr() {}
func (Array) expr()  {}
