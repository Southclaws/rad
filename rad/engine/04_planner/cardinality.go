package planner

import (
	"slices"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// refineUnique tightens a filter to at most one row when equality constraints
// pin every column of a primary key or unique index.
func (b *binder) refineUnique(filter *bound.Filter) {
	scan := underlyingScan(filter)
	if scan == nil {
		return
	}
	slotToColumn := map[lir.SlotID]string{}
	for _, field := range scan.Output().Fields {
		slotToColumn[field.Slot] = field.Name
	}

	pinned := map[string]bool{}
	var walk func(bound.Relation)
	collect := func(pred bound.Expr) {
		for _, conjunct := range conjuncts(pred) {
			binary, ok := conjunct.(bound.Binary)
			if !ok || binary.Op != lir.OpEq {
				continue
			}
			for _, side := range [2][2]bound.Expr{{binary.L, binary.R}, {binary.R, binary.L}} {
				ref, ok := side[0].(bound.SlotRef)
				if !ok {
					continue
				}
				column, belongsToScan := slotToColumn[ref.Slot]
				if belongsToScan && !readsAny(side[1], scan.Produced()) {
					pinned[column] = true
				}
			}
		}
	}
	walk = func(rel bound.Relation) {
		switch rel := rel.(type) {
		case *bound.Filter:
			collect(rel.Pred)
			walk(rel.In)
		case *bound.Order:
			walk(rel.In)
		case *bound.Slice:
			walk(rel.In)
		}
	}
	walk(filter)

	covers := func(columns []string) bool {
		if len(columns) == 0 {
			return false
		}
		for _, column := range columns {
			if !pinned[column] {
				return false
			}
		}
		return true
	}
	if covers(scan.Table.PrimaryKey) {
		filter.RefineCard(lir.Cardinality{Min: 0, Max: 1})
		return
	}
	for _, index := range scan.Table.Indexes {
		if index.Unique && covers(index.Columns) {
			filter.RefineCard(lir.Cardinality{Min: 0, Max: 1})
			return
		}
	}
}

func underlyingScan(rel bound.Relation) *bound.Scan {
	for {
		switch node := rel.(type) {
		case *bound.Scan:
			return node
		case *bound.Filter:
			rel = node.In
		case *bound.Order:
			rel = node.In
		case *bound.Slice:
			rel = node.In
		default:
			return nil
		}
	}
}

func conjuncts(expr bound.Expr) []bound.Expr {
	if binary, ok := expr.(bound.Binary); ok && binary.Op == lir.OpAnd {
		return append(conjuncts(binary.L), conjuncts(binary.R)...)
	}
	return []bound.Expr{expr}
}

func readsAny(expr bound.Expr, slots bound.SlotSet) bool {
	return slices.ContainsFunc(expr.FreeSlots().Slots(), slots.Contains)
}
