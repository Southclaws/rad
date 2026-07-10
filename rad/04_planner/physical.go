package planner

// The physical plan: what the executor runs. Logical bound IR names no
// indexes and no keys; these nodes do. The structural invariant carried
// over from v1, now visible in the tree: an access node only narrows which
// keys are scanned, and the FULL original predicate rides above it in a
// FilterExec — access-path choice can never change results.
//
// Ordering is a physical property here: scans declare what order they
// provide (ascending only — the KV has no reverse scan), and a SortExec
// exists only when the required ordering isn't already provided. Slices are
// lazy pull operators, so "stop early" is not a plan annotation — it is
// what a SliceExec over a non-blocking pipeline naturally does.

import (
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
)

// PhysNode is the sealed physical operator interface.
type PhysNode interface{ phys() }

// PhysPlan is a planned query: the operator tree, the root materialisation
// cardinality, and the output row type.
type PhysPlan struct {
	Root PhysNode
	Card qir.RootCard
	Out  qir.RowType
}

// PKGetExec fetches at most one row by primary key. Key values may be
// literals or outer-slot parameters resolved from the environment when the
// operator is built — the deduplicated to-parent pattern is a PKGetExec per
// distinct key.
type PKGetExec struct {
	Scan *bound.Scan
	Key  []ConstVal // one per primary-key column, in key order
}

// TableScanExec reads every row of a table, in primary-key order.
type TableScanExec struct {
	Scan *bound.Scan
}

// RangeSpec bounds one index column right after the equality prefix.
type RangeSpec struct {
	Column string
	Lo, Hi *RangeBound
}

// IndexRangeScanExec scans an index by equality prefix plus an optional
// trailing range, fetching base rows as it goes. It provides the index's
// remaining column order, then the primary key — ascending.
type IndexRangeScanExec struct {
	Scan     *bound.Scan
	Index    catalog.Index
	EqPrefix []ConstVal // one per leading index column
	Range    *RangeSpec
}

// FilterExec keeps rows where the predicate is TRUE. The predicate is
// always the full original conjunction — the structural residual.
type FilterExec struct {
	Input PhysNode
	Pred  bound.Expr
}

// CrossKind names which crossing a field materialises.
type CrossKind string

const (
	CrossExists CrossKind = "exists"
	CrossFirst  CrossKind = "first"
	CrossScalar CrossKind = "scalar"
	CrossArray  CrossKind = "array"
)

// AttachSpec is a projection field that materialises a sub-relation. The
// correlation classification decides the strategy: key-correlated attaches
// execute once per DISTINCT outer key over the batch (KeyedLookup — the
// deduplicated correlated execution); uncorrelated attaches execute once;
// general correlation falls back to per-row nested evaluation. All three
// are result-equivalent.
type AttachSpec struct {
	Kind CrossKind
	Corr Correlation
	Plan PhysNode
	Out  qir.RowType // the sub-relation's output shape
}

// PhysField is one projection output: either a scalar expression or an
// attached sub-relation, never both.
type PhysField struct {
	Name   string
	Slot   qir.SlotID
	Expr   bound.Expr
	Attach *AttachSpec
}

// ProjectExec assembles output rows — and is where nested materialisation
// lives. A crossing is never a scalar-evaluator callback: every attached
// field is an explicit sub-plan the executor can see.
type ProjectExec struct {
	Input  PhysNode
	Fields []PhysField
}

// SortExec is the blocking sort: stable, Value.Compare semantics (NULLs
// first ascending, last descending via negation).
type SortExec struct {
	Input PhysNode
	Terms []bound.OrderTerm
}

// SliceExec skips Offset rows and stops pulling after Limit accepted rows.
type SliceExec struct {
	Input  PhysNode
	Offset int
	Limit  *int
}

// NestedLoopJoinExec joins by materialising the right input and probing it
// per left row. Inner and left only.
type NestedLoopJoinExec struct {
	L, R PhysNode
	Kind qir.JoinKind
	On   bound.Expr
}

// AggregateExec folds its input: one row per distinct group, or exactly one
// row with no groups.
type AggregateExec struct {
	Input  PhysNode
	Groups []bound.GroupTerm
	Terms  []bound.AggTerm
}

func (*PKGetExec) phys()          {}
func (*TableScanExec) phys()      {}
func (*IndexRangeScanExec) phys() {}
func (*FilterExec) phys()         {}
func (*ProjectExec) phys()        {}
func (*SortExec) phys()           {}
func (*SliceExec) phys()          {}
func (*NestedLoopJoinExec) phys() {}
func (*AggregateExec) phys()      {}

// ── physical ordering properties ────────────────────────────────────────────

// providedOrder reports the ascending slot order a node's output arrives in,
// and whether the node is a singleton (at most one row satisfies any
// ordering). Filters and slices preserve their input's order; everything
// else provides none.
func providedOrder(n PhysNode) (slots []qir.SlotID, singleton bool) {
	switch x := n.(type) {
	case *PKGetExec:
		return nil, true
	case *TableScanExec:
		return pkSlots(x.Scan), false
	case *IndexRangeScanExec:
		var out []qir.SlotID
		for _, col := range x.Index.Columns[len(x.EqPrefix):] {
			if f, ok := x.Scan.Output().Lookup(col); ok {
				out = append(out, f.Slot)
			}
		}
		return append(out, pkSlots(x.Scan)...), false
	case *FilterExec:
		return providedOrder(x.Input)
	case *SliceExec:
		return providedOrder(x.Input)
	}
	return nil, false
}

func pkSlots(s *bound.Scan) []qir.SlotID {
	out := make([]qir.SlotID, 0, len(s.Table.PrimaryKey))
	for _, col := range s.Table.PrimaryKey {
		if f, ok := s.Output().Lookup(col); ok {
			out = append(out, f.Slot)
		}
	}
	return out
}

// satisfiesOrder reports whether the node's provided order already yields
// the required terms: every term ascending, each a bare slot reference
// matching the provided order as a prefix.
func satisfiesOrder(n PhysNode, req []bound.OrderTerm) bool {
	if len(req) == 0 {
		return true
	}
	provided, singleton := providedOrder(n)
	if singleton {
		return true
	}
	if len(req) > len(provided) {
		return false
	}
	for i, t := range req {
		if t.Desc {
			return false
		}
		ref, ok := t.Expr.(bound.SlotRef)
		if !ok || ref.Slot != provided[i] {
			return false
		}
	}
	return true
}
