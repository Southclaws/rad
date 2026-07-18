package planner

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// providedOrder reports the ascending slot order supplied by a node. A
// singleton satisfies every ordering because it contains no relative pair.
func providedOrder(n PhysNode) (slots []lir.SlotID, singleton bool) {
	switch x := n.(type) {
	case *PKGetExec:
		return nil, true
	case *TableScanExec:
		return pkSlots(x.Scan), false
	case *IndexRangeScanExec:
		var out []lir.SlotID
		for _, col := range x.Index.Columns[len(x.EqPrefix):] {
			if field, ok := x.Scan.Output().Lookup(col); ok {
				out = append(out, field.Slot)
			}
		}
		return append(out, pkSlots(x.Scan)...), false
	case *FilterExec:
		return providedOrder(x.Input)
	case *AttachExec:
		return providedOrder(x.Input)
	case *SliceExec:
		return providedOrder(x.Input)
	default:
		return nil, false
	}
}

func pkSlots(scan *bound.Scan) []lir.SlotID {
	out := make([]lir.SlotID, 0, len(scan.Table.PrimaryKey))
	for _, col := range scan.Table.PrimaryKey {
		if field, ok := scan.Output().Lookup(col); ok {
			out = append(out, field.Slot)
		}
	}
	return out
}

func satisfiesOrder(n PhysNode, required []bound.OrderTerm) bool {
	if len(required) == 0 {
		return true
	}
	provided, singleton := providedOrder(n)
	if singleton {
		return true
	}
	if len(required) > len(provided) {
		return false
	}
	for i, term := range required {
		if term.Desc {
			return false
		}
		ref, ok := term.Expr.(bound.SlotRef)
		if !ok || ref.Slot != provided[i] {
			return false
		}
	}
	return true
}
