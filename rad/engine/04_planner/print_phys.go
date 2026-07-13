package planner

import (
	"fmt"
	"strings"

	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// PrintPlan renders a physical plan as a deterministic indented tree for
// golden tests and EXPLAIN-style diagnostics.
func PrintPlan(p *PhysPlan) string {
	var b strings.Builder
	fmt.Fprintf(&b, "Plan card=%s\n", p.Card)
	for _, bp := range p.Bindings {
		sens := ""
		if bp.Sensitive {
			sens = " plan-choice-sensitive"
		}
		fmt.Fprintf(&b, "  Binding %s %s%s\n", bp.Name, bp.Strategy, sens)
		printPhys(&b, bp.Plan, 2)
	}
	printPhys(&b, p.Root, 1)
	return b.String()
}

func printPhys(b *strings.Builder, n PhysNode, depth int) {
	pad := strings.Repeat("  ", depth)
	switch x := n.(type) {
	case *PKGetExec:
		fmt.Fprintf(b, "%sPKGet %s [%s]\n", pad, x.Scan.Table.Name, keyEqs(x.Scan.Table.PrimaryKey, x.Key))
	case *TableScanExec:
		fmt.Fprintf(b, "%sTableScan %s\n", pad, x.Scan.Table.Name)
	case *IndexRangeScanExec:
		s := fmt.Sprintf("%sIndexRangeScan %s %s", pad, x.Scan.Table.Name, x.Index.Name)
		var parts []string
		if len(x.EqPrefix) > 0 {
			parts = append(parts, keyEqs(x.Index.Columns[:len(x.EqPrefix)], x.EqPrefix))
		}
		if x.Range != nil {
			parts = append(parts, rangeStr(x.Range))
		}
		if len(parts) > 0 {
			s += " [" + strings.Join(parts, ", ") + "]"
		}
		fmt.Fprintf(b, "%s\n", s)
	case *FilterExec:
		fmt.Fprintf(b, "%sFilter %s\n", pad, bound.PrintExpr(x.Pred))
		printPhys(b, x.Input, depth+1)
	case *RefExec:
		fmt.Fprintf(b, "%sRef %s\n", pad, x.Binding)
	case *AttachExec:
		fmt.Fprintf(b, "%sAttach\n", pad)
		for _, a := range x.Specs {
			fmt.Fprintf(b, "%s  #%d = %s %s%s\n", pad, a.Slot, a.Kind, a.Corr.Kind, corrKeys(a.Corr))
			printPhys(b, a.Plan, depth+2)
		}
		printPhys(b, x.Input, depth+1)
	case *ProjectExec:
		fmt.Fprintf(b, "%sProject\n", pad)
		for _, f := range x.Fields {
			fmt.Fprintf(b, "%s  %s#%d = %s\n", pad, f.Name, f.Slot, bound.PrintExpr(f.Expr))
		}
		printPhys(b, x.Input, depth+1)
	case *SortExec:
		terms := make([]string, len(x.Terms))
		for i, t := range x.Terms {
			dir := "asc"
			if t.Desc {
				dir = "desc"
			}
			terms[i] = bound.PrintExpr(t.Expr) + " " + dir
		}
		fmt.Fprintf(b, "%sSort %s\n", pad, strings.Join(terms, ", "))
		printPhys(b, x.Input, depth+1)
	case *SliceExec:
		lim := "∞"
		if x.Limit != nil {
			lim = fmt.Sprint(*x.Limit)
		}
		fmt.Fprintf(b, "%sSlice offset=%d limit=%s\n", pad, x.Offset, lim)
		printPhys(b, x.Input, depth+1)
	case *NestedLoopJoinExec:
		fmt.Fprintf(b, "%sNestedLoopJoin %s on %s\n", pad, x.Kind, bound.PrintExpr(x.On))
		printPhys(b, x.L, depth+1)
		printPhys(b, x.R, depth+1)
	case *AggregateExec:
		var parts []string
		for _, g := range x.Groups {
			parts = append(parts, fmt.Sprintf("group %s#%d=%s", g.Name, g.Slot, bound.PrintExpr(g.Expr)))
		}
		for _, t := range x.Terms {
			arg := "*"
			if t.Arg != nil {
				arg = bound.PrintExpr(t.Arg)
			}
			parts = append(parts, fmt.Sprintf("%s#%d=%s(%s)", t.Name, t.Slot, t.Fn, arg))
		}
		fmt.Fprintf(b, "%sAggregate %s\n", pad, strings.Join(parts, ", "))
		printPhys(b, x.Input, depth+1)
	default:
		fmt.Fprintf(b, "%s<%T>\n", pad, n)
	}
}

func keyEqs(cols []string, vals []ConstVal) string {
	parts := make([]string, len(vals))
	for i, v := range vals {
		parts[i] = cols[i] + " = " + constStr(v)
	}
	return strings.Join(parts, ", ")
}

func constStr(v ConstVal) string {
	if v.Lit != nil {
		return v.Lit.String()
	}
	return fmt.Sprintf("@%d", *v.Outer)
}

func rangeStr(r *RangeSpec) string {
	var parts []string
	if r.Lo != nil {
		op := ">"
		if r.Lo.Inclusive {
			op = ">="
		}
		parts = append(parts, fmt.Sprintf("%s %s %s", r.Column, op, r.Lo.V))
	}
	if r.Hi != nil {
		op := "<"
		if r.Hi.Inclusive {
			op = "<="
		}
		parts = append(parts, fmt.Sprintf("%s %s %s", r.Column, op, r.Hi.V))
	}
	return strings.Join(parts, ", ")
}

func corrKeys(c Correlation) string {
	if len(c.Keys) == 0 {
		return ""
	}
	parts := make([]string, len(c.Keys))
	for i, k := range c.Keys {
		parts[i] = fmt.Sprintf("%s#%d = @%d", k.InnerCol, k.InnerSlot, k.OuterSlot)
	}
	return " [" + strings.Join(parts, ", ") + "]"
}
