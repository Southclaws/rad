package planner

func physChildren(n PhysNode) []PhysNode {
	switch x := n.(type) {
	case *PKGetExec, *TableScanExec, *RowsExec, *IndexRangeScanExec, *RefExec, *RecursiveRefExec:
		return nil
	case *FilterExec:
		return []PhysNode{x.Input}
	case *AttachExec:
		children := make([]PhysNode, 0, len(x.Specs)+1)
		for _, spec := range x.Specs {
			children = append(children, spec.Plan)
		}
		return append(children, x.Input)
	case *ProjectExec:
		return []PhysNode{x.Input}
	case *SortExec:
		return []PhysNode{x.Input}
	case *SliceExec:
		return []PhysNode{x.Input}
	case *NestedLoopJoinExec:
		return []PhysNode{x.L, x.R}
	case *DistinctExec:
		return []PhysNode{x.Input}
	case *AggregateExec:
		return []PhysNode{x.Input}
	default:
		panic("planner: unknown physical node")
	}
}

func walkPhys(n PhysNode, visit func(PhysNode)) {
	visit(n)
	for _, child := range physChildren(n) {
		walkPhys(child, visit)
	}
}
