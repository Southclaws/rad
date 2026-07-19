package explain

// PlanView is the JSON-serialisable render of a physical plan — the query-plan
// view that rides the transport response as observability metadata (never part
// of the LIR/PIR IR). It is the single source of truth: the pretty String()
// below derives from it, so JSON clients render their own view and the CLI /
// devtool get the text from the same artifact.
//
// It captures the plan tree plus the access-path decision (candidates and
// scores).

import (
	"fmt"
	"strings"

	lirformat "github.com/Southclaws/rad/rad/engine/03_lir/format"
	"github.com/Southclaws/rad/rad/engine/04_planner/analysis"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
)

type PlanView struct {
	Cardinality string            `json:"cardinality"`
	Bindings    []PlanBindingView `json:"bindings,omitempty"`
	Root        *PlanNodeView     `json:"root"`
}

type PlanBindingView struct {
	Name                string        `json:"name"`
	Strategy            string        `json:"strategy"`
	PlanChoiceSensitive bool          `json:"planChoiceSensitive,omitempty"`
	Recursive           bool          `json:"recursive,omitempty"`
	Accumulation        string        `json:"accumulation,omitempty"`
	Plan                *PlanNodeView `json:"plan"`
	Step                *PlanNodeView `json:"step,omitempty"`
}

// PlanNodeView is one physical operator: its kind, a human summary of its
// distinguishing attributes, the access decision (on scan nodes), and inputs.
type PlanNodeView struct {
	Op       string                     `json:"op"`
	Detail   string                     `json:"detail,omitempty"`
	Access   []physical.AccessCandidate `json:"access,omitempty"`
	Children []*PlanNodeView            `json:"children,omitempty"`

	renderLines  []string
	renderAttach []planAttachRender
	renderInput  *PlanNodeView
}

type planAttachRender struct {
	header string
	plan   *PlanNodeView
}

// NewPlanView converts a physical plan into its view artifact.
func NewPlanView(p *physical.PhysPlan) *PlanView {
	v := &PlanView{Cardinality: string(p.Card), Root: viewNode(p.Root)}
	for _, bp := range p.Bindings {
		binding := PlanBindingView{
			Name:                bp.Name,
			Strategy:            string(bp.Strategy),
			PlanChoiceSensitive: bp.Sensitive,
			Plan:                viewNode(bp.Plan),
		}
		if bp.Recursive {
			binding.Recursive = true
			binding.Accumulation = string(bp.Accumulation)
			binding.Step = viewNode(bp.Step)
		}
		v.Bindings = append(v.Bindings, binding)
	}
	return v
}

func viewNode(n physical.PhysNode) *PlanNodeView {
	switch x := n.(type) {
	case *physical.PKGetExec:
		return &PlanNodeView{
			Op: "PKGet", Access: cands(x.Access),
			Detail: fmt.Sprintf("%s [%s]", x.Scan.Table.Name, keyEqs(x.Scan.Table.PrimaryKey, x.Key)),
		}
	case *physical.TableScanExec:
		return &PlanNodeView{Op: "TableScan", Detail: x.Scan.Table.Name, Access: cands(x.Access)}
	case *physical.RowsExec:
		return &PlanNodeView{Op: "Rows", Detail: fmt.Sprintf("×%d (%s)", len(x.Rows.Vals), x.Rows.Scope)}
	case *physical.IndexRangeScanExec:
		d := fmt.Sprintf("%s %s", x.Scan.Table.Name, x.Index.Name)
		var parts []string
		if len(x.EqPrefix) > 0 {
			parts = append(parts, keyEqs(x.Index.Columns[:len(x.EqPrefix)], x.EqPrefix))
		}
		if x.Range != nil {
			parts = append(parts, rangeStr(x.Range))
		}
		if len(parts) > 0 {
			d += " [" + strings.Join(parts, ", ") + "]"
		}
		return &PlanNodeView{Op: "IndexRangeScan", Detail: d, Access: cands(x.Access)}
	case *physical.FilterExec:
		return &PlanNodeView{Op: "Filter", Detail: lirformat.PrintExpr(x.Pred), Children: []*PlanNodeView{viewNode(x.Input)}}
	case *physical.RefExec:
		return &PlanNodeView{Op: "Ref", Detail: x.Binding}
	case *physical.RecursiveRefExec:
		return &PlanNodeView{Op: "RecursiveRef", Detail: x.Binding}
	case *physical.AttachExec:
		view := &PlanNodeView{Op: "Attach"}
		for _, a := range x.Specs {
			header := fmt.Sprintf("#%d = %s %s%s", a.Slot, a.Kind, a.Corr.Kind, corrKeys(a.Corr))
			plan := viewNode(a.Plan)
			spec := *plan
			spec.Detail = header + detailSuffix(plan.Detail)
			view.Children = append(view.Children, &spec)
			view.renderAttach = append(view.renderAttach, planAttachRender{header: header, plan: plan})
		}
		view.renderInput = viewNode(x.Input)
		view.Children = append(view.Children, view.renderInput)
		return view
	case *physical.ProjectExec:
		fields := make([]string, len(x.Fields))
		renderLines := make([]string, len(x.Fields))
		for i, f := range x.Fields {
			fields[i] = fmt.Sprintf("%s#%d=%s", f.Name, f.Slot, lirformat.PrintExpr(f.Expr))
			renderLines[i] = fmt.Sprintf("%s#%d = %s", f.Name, f.Slot, lirformat.PrintExpr(f.Expr))
		}
		return &PlanNodeView{
			Op: "Project", Detail: strings.Join(fields, ", "),
			Children: []*PlanNodeView{viewNode(x.Input)}, renderLines: renderLines,
		}
	case *physical.SortExec:
		terms := make([]string, len(x.Terms))
		for i, t := range x.Terms {
			dir := "asc"
			if t.Desc {
				dir = "desc"
			}
			terms[i] = lirformat.PrintExpr(t.Expr) + " " + dir
		}
		return &PlanNodeView{
			Op: "Sort", Detail: strings.Join(terms, ", "),
			Children: []*PlanNodeView{viewNode(x.Input)},
		}
	case *physical.SliceExec:
		lim := "∞"
		if x.Limit != nil {
			lim = fmt.Sprint(*x.Limit)
		}
		return &PlanNodeView{
			Op: "Slice", Detail: fmt.Sprintf("offset=%d limit=%s", x.Offset, lim),
			Children: []*PlanNodeView{viewNode(x.Input)},
		}
	case *physical.NestedLoopJoinExec:
		return &PlanNodeView{
			Op: "NestedLoopJoin", Detail: fmt.Sprintf("%s on %s", x.Kind, lirformat.PrintExpr(x.On)),
			Children: []*PlanNodeView{viewNode(x.L), viewNode(x.R)},
		}
	case *physical.AggregateExec:
		var parts []string
		for _, g := range x.Groups {
			parts = append(parts, fmt.Sprintf("group %s#%d=%s", g.Name, g.Slot, lirformat.PrintExpr(g.Expr)))
		}
		for _, t := range x.Terms {
			arg := "*"
			if t.Arg != nil {
				arg = lirformat.PrintExpr(t.Arg)
			}
			parts = append(parts, fmt.Sprintf("%s#%d=%s(%s)", t.Name, t.Slot, t.Fn, arg))
		}
		return &PlanNodeView{
			Op: "Aggregate", Detail: strings.Join(parts, ", "),
			Children: []*PlanNodeView{viewNode(x.Input)},
		}
	case *physical.DistinctExec:
		return &PlanNodeView{Op: "Distinct", Children: []*PlanNodeView{viewNode(x.Input)}}
	default:
		panic(fmt.Sprintf("planner: unknown physical node %T", n))
	}
}

func cands(a *physical.AccessDecision) []physical.AccessCandidate {
	if a == nil {
		return nil
	}
	return a.Candidates
}

func detailSuffix(s string) string {
	if s == "" {
		return ""
	}
	return " " + s
}

// PrintPlan renders a physical plan as a deterministic indented tree for
// golden tests and diagnostics.
func PrintPlan(p *physical.PhysPlan) string {
	return NewPlanView(p).render(false)
}

// String renders the plan view as a deterministic indented tree, including
// non-trivial access-path decisions.
func (v *PlanView) String() string {
	return v.render(true)
}

func (v *PlanView) render(showAccess bool) string {
	var b strings.Builder
	fmt.Fprintf(&b, "Plan card=%s\n", v.Cardinality)
	for _, bp := range v.Bindings {
		sens := ""
		if bp.PlanChoiceSensitive {
			sens = " plan-choice-sensitive"
		}
		if bp.Recursive {
			fmt.Fprintf(&b, "  Binding %s %s%s recursive accumulation=%s\n", bp.Name, bp.Strategy, sens, bp.Accumulation)
			fmt.Fprintln(&b, "    Anchor")
			writeNode(&b, bp.Plan, 3, showAccess)
			fmt.Fprintln(&b, "    Step")
			writeNode(&b, bp.Step, 3, showAccess)
			continue
		}
		fmt.Fprintf(&b, "  Binding %s %s%s\n", bp.Name, bp.Strategy, sens)
		writeNode(&b, bp.Plan, 2, showAccess)
	}
	writeNode(&b, v.Root, 1, showAccess)
	return b.String()
}

func writeNode(b *strings.Builder, n *PlanNodeView, depth int, showAccess bool) {
	if n == nil {
		return
	}
	pad := strings.Repeat("  ", depth)
	if len(n.renderAttach) > 0 {
		fmt.Fprintf(b, "%s%s\n", pad, n.Op)
		for _, attach := range n.renderAttach {
			fmt.Fprintf(b, "%s  %s\n", pad, attach.header)
			writeNode(b, attach.plan, depth+2, showAccess)
		}
		writeNode(b, n.renderInput, depth+1, showAccess)
		return
	}
	if len(n.renderLines) > 0 {
		fmt.Fprintf(b, "%s%s\n", pad, n.Op)
		for _, line := range n.renderLines {
			fmt.Fprintf(b, "%s  %s\n", pad, line)
		}
		for _, child := range n.Children {
			writeNode(b, child, depth+1, showAccess)
		}
		return
	}
	if n.Detail != "" {
		fmt.Fprintf(b, "%s%s %s\n", pad, n.Op, n.Detail)
	} else {
		fmt.Fprintf(b, "%s%s\n", pad, n.Op)
	}
	// Render the access decision only when the planner had a real choice
	// (an index or PK point-get won, or a scored index alternative existed).
	if showAccess {
		if a := accessLine(n.Access); a != "" {
			fmt.Fprintf(b, "%s  access: %s\n", pad, a)
		}
	}
	for _, c := range n.Children {
		writeNode(b, c, depth+1, showAccess)
	}
}

func accessLine(cands []physical.AccessCandidate) string {
	if len(cands) == 0 {
		return ""
	}
	// Suppress when a plain TableScan won and no alternative scored anything:
	// the planner had no real choice, so the line would be noise.
	var winner physical.AccessCandidate
	best := false
	for _, c := range cands {
		if c.Chosen {
			winner = c
		}
		if c.Score > 0 {
			best = true
		}
	}
	if winner.Method == "TableScan" && !best {
		return ""
	}
	parts := make([]string, len(cands))
	for i, c := range cands {
		mark := ""
		if c.Chosen {
			mark = " ✓"
		}
		if c.Method == "PKGet" {
			parts[i] = c.Method + mark
		} else {
			parts[i] = fmt.Sprintf("%s(%d)%s", c.Method, c.Score, mark)
		}
	}
	return strings.Join(parts, " · ")
}

func keyEqs(cols []string, vals []analysis.ConstVal) string {
	parts := make([]string, len(vals))
	for i, v := range vals {
		parts[i] = cols[i] + " = " + constStr(v)
	}
	return strings.Join(parts, ", ")
}

func constStr(v analysis.ConstVal) string {
	if v.Lit != nil {
		return v.Lit.String()
	}
	return fmt.Sprintf("@%d", *v.Outer)
}

func rangeStr(r *physical.RangeSpec) string {
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

func corrKeys(c analysis.Correlation) string {
	if len(c.Keys) == 0 {
		return ""
	}
	parts := make([]string, len(c.Keys))
	for i, k := range c.Keys {
		parts[i] = fmt.Sprintf("%s#%d = @%d", k.InnerCol, k.InnerSlot, k.OuterSlot)
	}
	return " [" + strings.Join(parts, ", ") + "]"
}
