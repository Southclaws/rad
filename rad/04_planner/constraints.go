package planner

// Constraint extraction: the analysis that feeds access-path selection.
// Given a relation that bottoms out at a scan through order-preserving
// wrappers, it reduces the filters' top-level AND conjuncts to per-column
// domains — pinned equalities and range bounds — that a physical planner
// can turn into key prefixes and scan bounds.
//
// This is analysis, not rewriting. There is deliberately no normalization
// pass: with binary AND and structural walkers, flattening or double-NOT
// elimination would only serve an extractor that this one doesn't need —
// anything it fails to recognise simply stays in the residual filter, which
// is always the full original predicate. OR trees, NOT, ne, and is_null
// contribute nothing to access paths by design.

import (
	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
)

// ConstVal is an equality's right-hand side that is fixed per evaluation of
// the relation: a literal, or an outer slot (a correlation key, whose value
// arrives when the enclosing row is known).
type ConstVal struct {
	Lit   *qir.Value
	Outer *qir.SlotID
}

// RangeBound is one end of a column range. Only literal bounds drive access
// paths this arc.
type RangeBound struct {
	V         qir.Value
	Inclusive bool
}

// Domain is everything the filters pin about one scan column.
type Domain struct {
	Eq     *ConstVal
	Lo, Hi *RangeBound
}

// ScanConstraints is the extraction result: the scan at the bottom of the
// chain and a domain per constrained column.
type ScanConstraints struct {
	Scan *bound.Scan
	Cols map[string]Domain
}

// ExtractConstraints analyses rel's filter chain. It returns nil when rel
// does not bottom out at a scan through Filter/Order/Slice wrappers —
// projections, joins, and aggregates have no single scan to constrain.
func ExtractConstraints(rel bound.Relation) *ScanConstraints {
	scan := underlyingScan(rel)
	if scan == nil {
		return nil
	}
	slotToCol := map[qir.SlotID]string{}
	for _, f := range scan.Output().Fields {
		slotToCol[f.Slot] = f.Name
	}

	cols := map[string]Domain{}
	poisoned := map[string]bool{} // conflicting equalities: leave to the residual filter

	visit := func(c bound.Expr) {
		bin, ok := c.(bound.Binary)
		if !ok {
			return
		}
		switch bin.Op {
		case qir.OpEq:
			for _, side := range [2][2]bound.Expr{{bin.L, bin.R}, {bin.R, bin.L}} {
				ref, ok := side[0].(bound.SlotRef)
				if !ok {
					continue
				}
				col, ours := slotToCol[ref.Slot]
				if !ours || poisoned[col] {
					continue
				}
				cv, ok := constVal(side[1], scan)
				if !ok {
					continue
				}
				d := cols[col]
				if d.Eq != nil && !sameConst(*d.Eq, cv) {
					// c = 1 AND c = 2: no index can serve both; the residual
					// filter yields the (empty) truth.
					poisoned[col] = true
					delete(cols, col)
					continue
				}
				// An equality pins the column exactly; range bounds seen in
				// either order are subsumed.
				d.Eq, d.Lo, d.Hi = &cv, nil, nil
				cols[col] = d
			}
		case qir.OpLt, qir.OpLte, qir.OpGt, qir.OpGte:
			// col OP lit, or lit OP col with the comparison flipped.
			if ref, ok := bin.L.(bound.SlotRef); ok {
				if lit, isLit := bin.R.(bound.Literal); isLit && !lit.V.Null {
					addRange(cols, poisoned, slotToCol, ref, bin.Op, lit.V)
				}
			} else if ref, ok := bin.R.(bound.SlotRef); ok {
				if lit, isLit := bin.L.(bound.Literal); isLit && !lit.V.Null {
					addRange(cols, poisoned, slotToCol, ref, flip(bin.Op), lit.V)
				}
			}
		}
	}

	for chain := rel; ; {
		switch n := chain.(type) {
		case *bound.Filter:
			for _, c := range conjuncts(n.Pred) {
				visit(c)
			}
			chain = n.In
		case *bound.Order:
			chain = n.In
		case *bound.Slice:
			chain = n.In
		default:
			return &ScanConstraints{Scan: scan, Cols: cols}
		}
	}
}

// constVal recognises a right-hand side fixed per evaluation: a non-NULL
// literal, or a slot the scan does not produce (an outer reference).
func constVal(e bound.Expr, scan *bound.Scan) (ConstVal, bool) {
	switch x := e.(type) {
	case bound.Literal:
		if x.V.Null {
			// eq NULL never matches; the residual filter handles it.
			return ConstVal{}, false
		}
		v := x.V
		return ConstVal{Lit: &v}, true
	case bound.SlotRef:
		if scan.Produced().Contains(x.Slot) {
			return ConstVal{}, false
		}
		s := x.Slot
		return ConstVal{Outer: &s}, true
	}
	return ConstVal{}, false
}

func sameConst(a, b ConstVal) bool {
	switch {
	case a.Lit != nil && b.Lit != nil:
		return a.Lit.Equal(*b.Lit)
	case a.Outer != nil && b.Outer != nil:
		return *a.Outer == *b.Outer
	}
	return false
}

func addRange(cols map[string]Domain, poisoned map[string]bool, slotToCol map[qir.SlotID]string, ref bound.SlotRef, op qir.BinaryOp, v qir.Value) {
	col, ours := slotToCol[ref.Slot]
	if !ours || poisoned[col] {
		return
	}
	d := cols[col]
	if d.Eq != nil {
		return // an equality already pins the column exactly
	}
	b := RangeBound{V: v, Inclusive: op == qir.OpLte || op == qir.OpGte}
	switch op {
	case qir.OpGt, qir.OpGte:
		if d.Lo == nil || tighterLo(b, *d.Lo) {
			d.Lo = &b
		}
	case qir.OpLt, qir.OpLte:
		if d.Hi == nil || tighterHi(b, *d.Hi) {
			d.Hi = &b
		}
	}
	cols[col] = d
}

// flip mirrors a comparison when the literal was on the left: 3 < col means
// col > 3.
func flip(op qir.BinaryOp) qir.BinaryOp {
	switch op {
	case qir.OpLt:
		return qir.OpGt
	case qir.OpLte:
		return qir.OpGte
	case qir.OpGt:
		return qir.OpLt
	default:
		return qir.OpLte
	}
}

func tighterLo(a, b RangeBound) bool {
	c, err := a.V.Compare(b.V)
	if err != nil {
		return false
	}
	return c > 0 || (c == 0 && !a.Inclusive && b.Inclusive)
}

func tighterHi(a, b RangeBound) bool {
	c, err := a.V.Compare(b.V)
	if err != nil {
		return false
	}
	return c < 0 || (c == 0 && !a.Inclusive && b.Inclusive)
}
