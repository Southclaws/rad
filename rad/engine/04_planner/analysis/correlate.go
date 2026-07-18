package analysis

// Correlation classification: the analysis behind deduplicated correlated
// execution. A crossing's relation is KEY-correlated when its only use of
// outer slots is equality conjuncts `scanColumn = outerSlot` in its filter
// chain — then the executor can dedupe the outer key tuples over a batch
// and fetch once per distinct key. Anything else that touches an outer slot
// (a range against it, an order term, a projected expression) makes the
// relation GENERAL-correlated, which falls back to per-row nested
// evaluation. Both strategies are result-equivalent; this analysis only
// picks the cheaper one.

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lirinspect "github.com/Southclaws/rad/rad/engine/03_lir/inspect"
)

// CorrKind classifies a relation's correlation.
type CorrKind int

const (
	Uncorrelated CorrKind = iota
	KeyCorrelated
	GeneralCorrelated
)

func (k CorrKind) String() string {
	switch k {
	case Uncorrelated:
		return "uncorrelated"
	case KeyCorrelated:
		return "key-correlated"
	default:
		return "general-correlated"
	}
}

// CorrKey is one correlation equation: the scan column (by slot and name)
// that must equal the outer slot's value.
type CorrKey struct {
	InnerSlot lir.SlotID
	InnerCol  string
	OuterSlot lir.SlotID
}

// Correlation is the classification result.
type Correlation struct {
	Kind CorrKind
	Keys []CorrKey
}

// Classify inspects a crossing's relation. Key-correlation additionally
// requires the chain to bottom out at a scan — the keys become that scan's
// equality constraints when the plan is instantiated per distinct key.
func Classify(rel bound.Relation) Correlation {
	free := rel.FreeSlots()
	if free.Empty() {
		return Correlation{Kind: Uncorrelated}
	}
	// The crossing may shape its result (Project) or fold it (Aggregate)
	// above the correlated filter — descend the single-input chain to the
	// scan the keys will constrain.
	scan := scanBelow(rel)
	if scan == nil {
		return Correlation{Kind: GeneralCorrelated}
	}
	slotToCol := map[lir.SlotID]string{}
	for _, f := range scan.Output().Fields {
		slotToCol[f.Slot] = f.Name
	}

	// Gather candidate key conjuncts from the filter chain.
	var candidates []CorrKey
	for chain := rel; chain != nil; {
		switch n := chain.(type) {
		case *bound.Filter:
			for _, c := range conjuncts(n.Pred) {
				bin, ok := c.(bound.Binary)
				if !ok || bin.Op != lir.OpEq {
					continue
				}
				for _, side := range [2][2]bound.Expr{{bin.L, bin.R}, {bin.R, bin.L}} {
					inner, ok := side[0].(bound.SlotRef)
					if !ok {
						continue
					}
					col, ours := slotToCol[inner.Slot]
					if !ours {
						continue
					}
					outer, ok := side[1].(bound.SlotRef)
					if !ok || rel.Produced().Contains(outer.Slot) {
						continue
					}
					candidates = append(candidates, CorrKey{InnerSlot: inner.Slot, InnerCol: col, OuterSlot: outer.Slot})
				}
			}
			chain = n.In
		case *bound.Order:
			chain = n.In
		case *bound.Slice:
			chain = n.In
		case *bound.Project:
			chain = n.In
		case *bound.Aggregate:
			chain = n.In
		case *bound.Distinct:
			chain = n.In
		default:
			chain = nil
		}
	}
	if len(candidates) == 0 {
		return Correlation{Kind: GeneralCorrelated}
	}

	// Every use of an outer slot must be one of the candidate conjuncts —
	// count usages everywhere, subtract the candidates', and anything left
	// means general correlation.
	usage := map[lir.SlotID]int{}
	countFreeUsage(rel, free, usage)
	for _, k := range candidates {
		usage[k.OuterSlot]--
	}
	for _, n := range usage {
		if n > 0 {
			return Correlation{Kind: GeneralCorrelated}
		}
	}
	return Correlation{Kind: KeyCorrelated, Keys: candidates}
}

// scanBelow descends operators that preserve a single underlying scan.
func scanBelow(rel bound.Relation) *bound.Scan {
	for {
		switch n := rel.(type) {
		case *bound.Scan:
			return n
		case *bound.Filter:
			rel = n.In
		case *bound.Order:
			rel = n.In
		case *bound.Slice:
			rel = n.In
		case *bound.Project:
			rel = n.In
		case *bound.Aggregate:
			rel = n.In
		case *bound.Distinct:
			rel = n.In
		default:
			return nil
		}
	}
}

// countFreeUsage tallies references to the relation's free slots. Slots owned
// by a nested crossing are local to that crossing even though they are not
// produced by the enclosing relation.
func countFreeUsage(rel bound.Relation, free bound.SlotSet, usage map[lir.SlotID]int) {
	lirinspect.WalkRelation(rel, nil, func(expr bound.Expr) {
		ref, ok := expr.(bound.SlotRef)
		if ok && free.Contains(ref.Slot) {
			usage[ref.Slot]++
		}
	})
}
