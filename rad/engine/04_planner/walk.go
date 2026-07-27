package planner

import "github.com/Southclaws/rad/rad/engine/04_planner/physical"

func physChildren(n physical.PhysNode) []physical.PhysNode {
	switch x := n.(type) {
	case *physical.PKGetExec, *physical.TableScanExec, *physical.RowsExec, *physical.IndexRangeScanExec, *physical.RefExec, *physical.RecursiveRefExec:
		return nil
	case *physical.FilterExec:
		return []physical.PhysNode{x.Input}
	case *physical.AttachExec:
		children := make([]physical.PhysNode, 0, len(x.Specs)+1)
		for _, spec := range x.Specs {
			children = append(children, spec.Plan)
		}
		return append(children, x.Input)
	case *physical.ProjectExec:
		return []physical.PhysNode{x.Input}
	case *physical.SortExec:
		return []physical.PhysNode{x.Input}
	case *physical.SliceExec:
		return []physical.PhysNode{x.Input}
	case *physical.NestedLoopJoinExec:
		return []physical.PhysNode{x.L, x.R}
	case *physical.ConcatenateExec:
		return x.Ins
	case *physical.IntersectExec:
		return []physical.PhysNode{x.L, x.R}
	case *physical.ExceptExec:
		return []physical.PhysNode{x.L, x.R}
	case *physical.DistinctExec:
		return []physical.PhysNode{x.Input}
	case *physical.AggregateExec:
		return []physical.PhysNode{x.Input}
	default:
		panic("planner: unknown physical node")
	}
}

func walkPhys(n physical.PhysNode, visit func(physical.PhysNode)) {
	visit(n)
	for _, child := range physChildren(n) {
		walkPhys(child, visit)
	}
}
