package format

import (
	"fmt"
	"strings"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// Print renders a bound query as a deterministic indented tree, for golden
// tests and diagnostics. Slots print as name#id so a snapshot pins both the
// resolution and the allocation.
func Print(q *bound.Query) string {
	var b strings.Builder
	fmt.Fprintf(&b, "Query card=%s\n", q.Card)
	for _, bnd := range q.Bindings {
		sens := ""
		if bnd.PlanSensitive {
			sens = " plan-choice-sensitive"
		}
		if bnd.Recursive {
			fmt.Fprintf(&b, "  Binding %s recursive accumulation=%s%s\n", bnd.Name, bnd.Accumulation, sens)
			fmt.Fprintf(&b, "    anchor\n")
			printRel(&b, bnd.Root, 3)
			fmt.Fprintf(&b, "    step\n")
			printRel(&b, bnd.Step, 3)
			continue
		}
		fmt.Fprintf(&b, "  Binding %s%s\n", bnd.Name, sens)
		printRel(&b, bnd.Root, 2)
	}
	printRel(&b, q.Root, 1)
	return b.String()
}

func printRel(b *strings.Builder, rel bound.Relation, depth int) {
	pad := strings.Repeat("  ", depth)
	switch n := rel.(type) {
	case *bound.Scan:
		fmt.Fprintf(b, "%sScan %s (%s) %s\n", pad, n.Table.Name, n.Scope, rowType(n.Output()))
	case *bound.Rows:
		fmt.Fprintf(b, "%sRows ×%d (%s) %s\n", pad, len(n.Vals), n.Scope, rowType(n.Output()))
	case *bound.Ref:
		fmt.Fprintf(b, "%sRef %s (%s) %s\n", pad, n.Binding, n.Scope, rowType(n.Output()))
	case *bound.RecursiveRef:
		fmt.Fprintf(b, "%sRecursiveRef %s (%s) %s\n", pad, n.Binding, n.Scope, rowType(n.Output()))
	case *bound.Distinct:
		fmt.Fprintf(b, "%sDistinct%s\n", pad, suffix(n))
		printRel(b, n.In, depth+1)
	case *bound.Filter:
		fmt.Fprintf(b, "%sFilter %s%s\n", pad, printExpr(n.Pred), suffix(n))
		printRel(b, n.In, depth+1)
	case *bound.Project:
		fmt.Fprintf(b, "%sProject%s\n", pad, suffix(n))
		for _, f := range n.Fields {
			fmt.Fprintf(b, "%s  %s#%d = %s : %s\n", pad, f.Name, f.Slot, printExpr(f.Expr), f.Expr.Type())
		}
		printRel(b, n.In, depth+1)
	case *bound.Join:
		fmt.Fprintf(b, "%sJoin %s on %s%s\n", pad, n.Kind, printExpr(n.On), suffix(n))
		printRel(b, n.L, depth+1)
		printRel(b, n.R, depth+1)
	case *bound.Aggregate:
		fmt.Fprintf(b, "%sAggregate%s\n", pad, suffix(n))
		for _, g := range n.Groups {
			fmt.Fprintf(b, "%s  group %s#%d = %s\n", pad, g.Name, g.Slot, printExpr(g.Expr))
		}
		for _, t := range n.Terms {
			arg := "*"
			if t.Arg != nil {
				arg = printExpr(t.Arg)
			}
			fmt.Fprintf(b, "%s  %s#%d = %s(%s) : %s\n", pad, t.Name, t.Slot, t.Fn, arg, t.T)
		}
		printRel(b, n.In, depth+1)
	case *bound.Order:
		terms := make([]string, len(n.Terms))
		for i, t := range n.Terms {
			dir := "asc"
			if t.Desc {
				dir = "desc"
			}
			terms[i] = printExpr(t.Expr) + " " + dir
		}
		fmt.Fprintf(b, "%sOrder %s%s\n", pad, strings.Join(terms, ", "), suffix(n))
		printRel(b, n.In, depth+1)
	case *bound.Slice:
		lim := "∞"
		if n.Limit != nil {
			lim = fmt.Sprint(*n.Limit)
		}
		fmt.Fprintf(b, "%sSlice offset=%d limit=%s%s\n", pad, n.Offset, lim, suffix(n))
		printRel(b, n.In, depth+1)
	default:
		fmt.Fprintf(b, "%s<%T>\n", pad, rel)
	}
}

// suffix renders the interesting laws: cardinality always, free slots when
// the relation is correlated.
func suffix(rel bound.Relation) string {
	s := fmt.Sprintf("  [card %s", rel.Card())
	if free := rel.FreeSlots(); !free.Empty() {
		ids := make([]string, 0, 4)
		for _, id := range free.Slots() {
			ids = append(ids, fmt.Sprint(id))
		}
		s += " free " + strings.Join(ids, ",")
	}
	return s + "]"
}

// PrintExpr renders one bound expression on a line, for plan printers and
// diagnostics.
func PrintExpr(e bound.Expr) string { return printExpr(e) }

func printExpr(e bound.Expr) string {
	switch x := e.(type) {
	case bound.Literal:
		return x.V.String()
	case bound.SlotRef:
		return fmt.Sprintf("%s#%d", x.Name, x.Slot)
	case bound.Unary:
		return fmt.Sprintf("%s(%s)", x.Op, printExpr(x.X))
	case bound.Binary:
		return fmt.Sprintf("%s(%s, %s)", x.Op, printExpr(x.L), printExpr(x.R))
	case bound.Cast:
		return fmt.Sprintf("cast(%s as %s)", printExpr(x.X), x.To)
	case bound.Exists:
		return "exists(" + subrel(x.Rel) + ")"
	case bound.First:
		return "first(" + subrel(x.Rel) + ")"
	case bound.Scalar:
		return "scalar(" + subrel(x.Rel) + ")"
	case bound.Array:
		return "array(" + subrel(x.Rel) + ")"
	}
	return fmt.Sprintf("<%T>", e)
}

// subrel renders a crossing's relation inline, indented under the owner.
func subrel(rel bound.Relation) string {
	var b strings.Builder
	b.WriteString("\n")
	printRel(&b, rel, 3)
	return strings.TrimRight(b.String(), "\n") + "\n    "
}

func rowType(rt lir.RowType) string {
	parts := make([]string, len(rt.Fields))
	for i, f := range rt.Fields {
		parts[i] = fmt.Sprintf("%s#%d:%s", f.Name, f.Slot, f.Type)
	}
	return "{" + strings.Join(parts, " ") + "}"
}
