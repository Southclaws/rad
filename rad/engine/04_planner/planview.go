package planner

// PlanView is the JSON-serialisable render of a physical plan — the query-plan
// view that rides the transport response as observability metadata (never part
// of the LIR/PIR IR). It is the single source of truth: the pretty String()
// below derives from it, so JSON clients render their own view and the CLI /
// devtool get the text from the same artifact.
//
// It captures the plan tree plus the access-path decision (candidates and
// scores). Actual-row metrics, spans, and rewrite history are not yet
// represented.

import (
	"fmt"
	"strings"

	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
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
	Plan                *PlanNodeView `json:"plan"`
}

// PlanNodeView is one physical operator: its kind, a human summary of its
// distinguishing attributes, the access decision (on scan nodes), and inputs.
type PlanNodeView struct {
	Op       string            `json:"op"`
	Detail   string            `json:"detail,omitempty"`
	Access   []AccessCandidate `json:"access,omitempty"`
	Children []*PlanNodeView   `json:"children,omitempty"`
}

// NewPlanView converts a physical plan into its view artifact.
func NewPlanView(p *PhysPlan) *PlanView {
	v := &PlanView{Cardinality: string(p.Card), Root: viewNode(p.Root)}
	for _, bp := range p.Bindings {
		v.Bindings = append(v.Bindings, PlanBindingView{
			Name:                bp.Name,
			Strategy:            string(bp.Strategy),
			PlanChoiceSensitive: bp.Sensitive,
			Plan:                viewNode(bp.Plan),
		})
	}
	return v
}

func viewNode(n PhysNode) *PlanNodeView {
	switch x := n.(type) {
	case *PKGetExec:
		return &PlanNodeView{Op: "PKGet", Access: cands(x.Access),
			Detail: fmt.Sprintf("%s [%s]", x.Scan.Table.Name, keyEqs(x.Scan.Table.PrimaryKey, x.Key))}
	case *TableScanExec:
		return &PlanNodeView{Op: "TableScan", Detail: x.Scan.Table.Name, Access: cands(x.Access)}
	case *RowsExec:
		return &PlanNodeView{Op: "Rows", Detail: fmt.Sprintf("×%d (%s)", len(x.Rows.Vals), x.Rows.Scope)}
	case *IndexRangeScanExec:
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
	case *FilterExec:
		return &PlanNodeView{Op: "Filter", Detail: bound.PrintExpr(x.Pred), Children: []*PlanNodeView{viewNode(x.Input)}}
	case *RefExec:
		return &PlanNodeView{Op: "Ref", Detail: x.Binding}
	case *AttachExec:
		n := &PlanNodeView{Op: "Attach"}
		for _, a := range x.Specs {
			spec := viewNode(a.Plan)
			spec.Detail = fmt.Sprintf("#%d = %s %s%s", a.Slot, a.Kind, a.Corr.Kind, corrKeys(a.Corr)) +
				detailSuffix(spec.Detail)
			n.Children = append(n.Children, spec)
		}
		n.Children = append(n.Children, viewNode(x.Input))
		return n
	case *ProjectExec:
		fields := make([]string, len(x.Fields))
		for i, f := range x.Fields {
			fields[i] = fmt.Sprintf("%s#%d=%s", f.Name, f.Slot, bound.PrintExpr(f.Expr))
		}
		return &PlanNodeView{Op: "Project", Detail: strings.Join(fields, ", "),
			Children: []*PlanNodeView{viewNode(x.Input)}}
	case *SortExec:
		terms := make([]string, len(x.Terms))
		for i, t := range x.Terms {
			dir := "asc"
			if t.Desc {
				dir = "desc"
			}
			terms[i] = bound.PrintExpr(t.Expr) + " " + dir
		}
		return &PlanNodeView{Op: "Sort", Detail: strings.Join(terms, ", "),
			Children: []*PlanNodeView{viewNode(x.Input)}}
	case *SliceExec:
		lim := "∞"
		if x.Limit != nil {
			lim = fmt.Sprint(*x.Limit)
		}
		return &PlanNodeView{Op: "Slice", Detail: fmt.Sprintf("offset=%d limit=%s", x.Offset, lim),
			Children: []*PlanNodeView{viewNode(x.Input)}}
	case *NestedLoopJoinExec:
		return &PlanNodeView{Op: "NestedLoopJoin", Detail: fmt.Sprintf("%s on %s", x.Kind, bound.PrintExpr(x.On)),
			Children: []*PlanNodeView{viewNode(x.L), viewNode(x.R)}}
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
		return &PlanNodeView{Op: "Aggregate", Detail: strings.Join(parts, ", "),
			Children: []*PlanNodeView{viewNode(x.Input)}}
	default:
		return &PlanNodeView{Op: fmt.Sprintf("%T", n)}
	}
}

func cands(a *AccessDecision) []AccessCandidate {
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

// String renders the plan view as a deterministic indented tree, including the
// access-path decision under each scan that had a non-trivial choice.
func (v *PlanView) String() string {
	var b strings.Builder
	fmt.Fprintf(&b, "Plan card=%s\n", v.Cardinality)
	for _, bp := range v.Bindings {
		sens := ""
		if bp.PlanChoiceSensitive {
			sens = " plan-choice-sensitive"
		}
		fmt.Fprintf(&b, "  Binding %s %s%s\n", bp.Name, bp.Strategy, sens)
		writeNode(&b, bp.Plan, 2)
	}
	writeNode(&b, v.Root, 1)
	return b.String()
}

func writeNode(b *strings.Builder, n *PlanNodeView, depth int) {
	if n == nil {
		return
	}
	pad := strings.Repeat("  ", depth)
	if n.Detail != "" {
		fmt.Fprintf(b, "%s%s %s\n", pad, n.Op, n.Detail)
	} else {
		fmt.Fprintf(b, "%s%s\n", pad, n.Op)
	}
	// Render the access decision only when the planner had a real choice
	// (an index or PK point-get won, or a scored index alternative existed).
	if a := accessLine(n.Access); a != "" {
		fmt.Fprintf(b, "%s  access: %s\n", pad, a)
	}
	for _, c := range n.Children {
		writeNode(b, c, depth+1)
	}
}

func accessLine(cands []AccessCandidate) string {
	if len(cands) == 0 {
		return ""
	}
	// Suppress when a plain TableScan won and no alternative scored anything:
	// the planner had no real choice, so the line would be noise.
	var winner AccessCandidate
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
