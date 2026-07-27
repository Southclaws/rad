package planner

import (
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
)

// prepareCatalogDependencies annotates every physical access with the cells it
// must decode, then records the exact compatibility fences the completed plan
// relies upon. This happens after access selection because only the selected
// physical index is an index-access dependency.
func prepareCatalogDependencies(plan *physical.PhysPlan) {
	required := requiredSlots(plan)
	var dependencies model.CatalogDependencies

	visit := func(node physical.PhysNode) {
		switch access := node.(type) {
		case *physical.PKGetExec:
			access.DecodeColumns = decodeColumns(access.Scan, required)
			columns := slices.Clone(access.DecodeColumns)
			columns = appendNamedColumns(columns, access.Scan.Table, access.Scan.Table.PrimaryKey...)
			dependencies.AddTableRead(access.Scan.Table, columns)

		case *physical.TableScanExec:
			access.DecodeColumns = decodeColumns(access.Scan, required)
			dependencies.AddTableRead(access.Scan.Table, access.DecodeColumns)

		case *physical.IndexRangeScanExec:
			access.DecodeColumns = decodeColumns(access.Scan, required)
			columns := slices.Clone(access.DecodeColumns)
			columns = appendNamedColumns(columns, access.Scan.Table, access.Index.Columns...)
			dependencies.AddIndexRead(access.Scan.Table, access.Index, columns)
		}
	}
	walkPlan(plan, visit)
	plan.Dependencies = dependencies
}

func walkPlan(plan *physical.PhysPlan, visit func(physical.PhysNode)) {
	for _, binding := range plan.Bindings {
		walkPhys(binding.Plan, visit)
		if binding.Recursive {
			walkPhys(binding.Step, visit)
		}
	}
	walkPhys(plan.Root, visit)
}

// requiredSlots collects every scalar value the physical semantics observe.
// Most pass-through operators need no special treatment: the final output and
// expressions above them identify the live slots. Operators that compare or
// remap complete rows add those row shapes explicitly.
func requiredSlots(plan *physical.PhysPlan) bound.SlotSet {
	required := bound.NewSlotSet(plan.Out.Slots()...)
	addRow := func(row lir.RowType) {
		required = required.Union(bound.NewSlotSet(row.Slots()...))
	}
	addExpr := func(expr bound.Expr) {
		if expr != nil {
			required = required.Union(expr.FreeSlots())
		}
	}

	for _, binding := range plan.Bindings {
		// Binding values are a reusable relational contract. Narrowing them
		// across Ref remapping belongs to a later inter-binding liveness pass.
		addRow(binding.Out)
		if binding.Recursive {
			addRow(binding.StepOut)
		}
	}

	walkPlan(plan, func(node physical.PhysNode) {
		switch n := node.(type) {
		case *physical.FilterExec:
			addExpr(n.Pred)
		case *physical.AttachExec:
			for _, spec := range n.Specs {
				addRow(spec.Out)
			}
		case *physical.ProjectExec:
			for _, field := range n.Fields {
				addExpr(field.Expr)
			}
		case *physical.SortExec:
			for _, term := range n.Terms {
				addExpr(term.Expr)
			}
		case *physical.NestedLoopJoinExec:
			addExpr(n.On)
		case *physical.ConcatenateExec:
			for _, out := range n.InOuts {
				addRow(out)
			}
		case *physical.IntersectExec:
			addRow(n.LOut)
			addRow(n.ROut)
		case *physical.ExceptExec:
			addRow(n.LOut)
			addRow(n.ROut)
		case *physical.DistinctExec:
			addRow(n.Out)
		case *physical.AggregateExec:
			for _, group := range n.Groups {
				addExpr(group.Expr)
			}
			for _, term := range n.Terms {
				addExpr(term.Arg)
			}
		}
	})
	return required
}

func decodeColumns(scan *bound.Scan, required bound.SlotSet) []model.Column {
	columns := make([]model.Column, 0, len(scan.Table.Columns))
	for i, field := range scan.Output().Fields {
		if required.Contains(field.Slot) {
			columns = append(columns, scan.Table.Columns[i])
		}
	}
	return columns
}

func appendNamedColumns(columns []model.Column, table model.Table, names ...string) []model.Column {
	for _, name := range names {
		column, ok := table.Column(name)
		if !ok {
			panic("planner: bound access references a missing column")
		}
		if slices.ContainsFunc(columns, func(existing model.Column) bool { return existing.ID == column.ID }) {
			continue
		}
		columns = append(columns, column)
	}
	return columns
}
