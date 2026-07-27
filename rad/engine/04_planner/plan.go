package planner

// Physical planning: bound IR to the operator tree. Pure — everything was
// resolved and validated by the binder, so planning cannot fail on client
// input; it only chooses implementations.
//
// Access selection picks the cheapest path the constraints support: a fully
// pinned primary key wins outright; otherwise indexes rank by leading
// equality-prefix length, then a trailing range bound, then whether the
// index's provided ordering satisfies the required ordering (the whole of
// the limited ordered-index pushdown policy); otherwise a table scan. The
// full predicate always rides above the access node.

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/04_planner/analysis"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
)

// PlanOpt adjusts planning; the conformance suite uses these to prove that
// physical choices never change results.
type Option func(*planCfg)

type planCfg struct {
	fullScanOnly bool
}

// FullScanOnly disables point gets and index scans: every access is a table
// scan filtered by the full predicate. Results must be identical to the
// chosen plan's — the residual filter is the source of truth.
func FullScanOnly() Option {
	return func(c *planCfg) { c.fullScanOnly = true }
}

// PlanQuery lowers a bound query into a physical plan.
func PlanQuery(q *bound.Query, opts ...Option) *physical.PhysPlan {
	cfg := planCfg{}
	for _, o := range opts {
		o(&cfg)
	}
	pl := &planner{cfg: cfg, nextSlot: q.Slots}
	bindings := make([]physical.BindingPlan, len(q.Bindings))
	for i, b := range q.Bindings {
		bp := physical.BindingPlan{
			Name:      b.Name,
			Plan:      pl.plan(b.Root, nil),
			Out:       b.Root.Output(),
			Sensitive: b.PlanSensitive,
		}
		if b.Recursive {
			bp.Recursive = true
			bp.Accumulation = b.Accumulation
			bp.Step = pl.plan(b.Step, nil)
			bp.StepOut = b.Step.Output()
			bp.Out = b.Out
		}
		bindings[i] = bp
	}
	root := pl.plan(q.Root, nil)

	// Strategy: a binding with exactly one occurrence streams inline at
	// that occurrence (its single evaluation IS the commitment); anything
	// referenced more than once materialises, so nested multi-reference
	// bindings execute linearly instead of exponentially re-running their
	// children. A recursive binding always materialises — its fixpoint is
	// computed before the root runs, and its step's recursive_ref is not an
	// occurrence of the committed value.
	refs := map[string]int{}
	countRefs(root, refs)
	for i := range bindings {
		countRefs(bindings[i].Plan, refs)
		if bindings[i].Recursive {
			countRefs(bindings[i].Step, refs)
		}
	}
	for i := range bindings {
		switch {
		case bindings[i].Recursive:
			bindings[i].Strategy = physical.BindingMaterialise
		case refs[bindings[i].Name] == 1:
			bindings[i].Strategy = physical.BindingReplay
		default:
			bindings[i].Strategy = physical.BindingMaterialise
		}
	}

	plan := &physical.PhysPlan{
		Bindings: bindings,
		Root:     root,
		Card:     q.Card,
		Out:      q.Root.Output(),
		Slots:    pl.nextSlot,
	}
	prepareCatalogDependencies(plan)
	return plan
}

// countRefs tallies RefExec occurrences per binding across a plan tree,
// including plans attached for extracted crossings.
func countRefs(n physical.PhysNode, refs map[string]int) {
	walkPhys(n, func(n physical.PhysNode) {
		if ref, ok := n.(*physical.RefExec); ok {
			refs[ref.Binding]++
		}
	})
}

type planner struct {
	cfg planCfg
	// nextSlot continues the binder's dense slot space: every extracted
	// crossing gets a fresh slot its rewritten expression references.
	nextSlot lir.SlotID
}

func (pl *planner) allocSlot() lir.SlotID {
	s := pl.nextSlot
	pl.nextSlot++
	return s
}

// plan lowers one relation. req is the ordering an enclosing Order needs —
// flowing it down lets access selection prefer an order-providing index, so
// the Order above can disappear entirely.
func (pl *planner) plan(rel bound.Relation, req []bound.OrderTerm) physical.PhysNode {
	switch n := rel.(type) {
	case *bound.Scan:
		return pl.chooseAccessPath(&analysis.ScanConstraints{Scan: n, Cols: map[string]analysis.Domain{}}, req)

	case *bound.Rows:
		return &physical.RowsExec{Rows: n}

	case *bound.Filter:
		// A filter chain over a scan plans as a unit: constraints choose the
		// access path, and the merged conjunction rides above it. Crossings
		// contribute no constraints; they extract out of the residual and
		// attach beneath it.
		if scan := analysis.UnderlyingScan(n); scan != nil && filterChain(n) {
			cs := analysis.ExtractConstraints(n)
			access := pl.chooseAccessPath(cs, req)
			pred, specs := pl.extractExpr(mergedPred(n))
			return &physical.FilterExec{Input: attachWrap(access, specs), Pred: pred}
		}
		pred, specs := pl.extractExpr(n.Pred)
		return &physical.FilterExec{Input: attachWrap(pl.plan(n.In, req), specs), Pred: pred}

	case *bound.Order:
		in := pl.plan(n.In, n.Terms)
		terms := make([]bound.OrderTerm, len(n.Terms))
		var specs []*physical.AttachSpec
		for i, t := range n.Terms {
			e, s := pl.extractExpr(t.Expr)
			terms[i] = bound.OrderTerm{Expr: e, Desc: t.Desc}
			specs = append(specs, s...)
		}
		if len(specs) == 0 && satisfiesOrder(in, terms) {
			return in
		}
		return &physical.SortExec{Input: attachWrap(in, specs), Terms: terms}

	case *bound.Slice:
		// Ordering does not commute with slicing, so nothing flows through.
		return &physical.SliceExec{Input: pl.plan(n.In, nil), Offset: n.Offset, Limit: n.Limit}

	case *bound.Project:
		fields := make([]physical.PhysField, len(n.Fields))
		var specs []*physical.AttachSpec
		for i, f := range n.Fields {
			e, s := pl.extractField(f)
			fields[i] = physical.PhysField{Name: f.Name, Slot: f.Slot, Expr: e}
			specs = append(specs, s...)
		}
		return &physical.ProjectExec{Input: attachWrap(pl.plan(n.In, nil), specs), Fields: fields}

	case *bound.Aggregate:
		groups := make([]bound.GroupTerm, len(n.Groups))
		var specs []*physical.AttachSpec
		for i, g := range n.Groups {
			e, s := pl.extractExpr(g.Expr)
			groups[i] = bound.GroupTerm{Name: g.Name, Slot: g.Slot, Expr: e}
			specs = append(specs, s...)
		}
		terms := make([]bound.AggTerm, len(n.Terms))
		for i, t := range n.Terms {
			terms[i] = t
			if t.Arg != nil {
				e, s := pl.extractExpr(t.Arg)
				terms[i].Arg = e
				specs = append(specs, s...)
			}
		}
		return &physical.AggregateExec{Input: attachWrap(pl.plan(n.In, nil), specs), Groups: groups, Terms: terms}

	case *bound.Join:
		// The binder rejects crossings in join conditions, so On is pure.
		return &physical.NestedLoopJoinExec{L: pl.plan(n.L, nil), R: pl.plan(n.R, nil), Kind: n.Kind, On: n.On, ROut: n.R.Output()}

	case *bound.Concatenate:
		ins := make([]physical.PhysNode, len(n.Ins))
		inOuts := make([]lir.RowType, len(n.Ins))
		for i, in := range n.Ins {
			ins[i] = pl.plan(in, nil)
			inOuts[i] = in.Output()
		}
		return &physical.ConcatenateExec{Ins: ins, InOuts: inOuts, Out: n.Output()}

	case *bound.Intersect:
		return &physical.IntersectExec{
			L: pl.plan(n.L, nil), R: pl.plan(n.R, nil),
			Quantifier: n.Quantifier, LOut: n.L.Output(), ROut: n.R.Output(), Out: n.Output(),
		}

	case *bound.Except:
		return &physical.ExceptExec{
			L: pl.plan(n.L, nil), R: pl.plan(n.R, nil),
			Quantifier: n.Quantifier, LOut: n.L.Output(), ROut: n.R.Output(), Out: n.Output(),
		}

	case *bound.Ref:
		return &physical.RefExec{Binding: n.Binding, Out: n.Output(), Canon: n.Canon}

	case *bound.RecursiveRef:
		return &physical.RecursiveRefExec{Binding: n.Binding, Out: n.Output(), Canon: n.Canon}

	case *bound.Distinct:
		return &physical.DistinctExec{Input: pl.plan(n.In, nil), Out: n.Output()}
	}
	panic("planner: unplannable relation") // sealed interface; unreachable
}

// extractField extracts a projection field's crossings. A field that IS a
// crossing keeps its own slot — the attach writes the field directly and the
// expression is a self-reference the executor recognises as already present.
func (pl *planner) extractField(f bound.ProjField) (bound.Expr, []*physical.AttachSpec) {
	if kind, rel, ok := crossing(f.Expr); ok {
		spec := pl.attachSpec(f.Slot, kind, rel)
		return bound.SlotRef{Slot: f.Slot, Name: f.Name, T: f.Expr.Type()}, []*physical.AttachSpec{spec}
	}
	return pl.extractExpr(f.Expr)
}

// extractExpr rewrites e so no crossing remains: each becomes an AttachSpec
// writing a fresh slot, and the expression references the slot. Whatever
// wraps the crossing — a NOT, an arithmetic term, a comparison — evaluates
// over the slot, so wrapping never changes how the sub-relation executes.
func (pl *planner) extractExpr(e bound.Expr) (bound.Expr, []*physical.AttachSpec) {
	if kind, rel, ok := crossing(e); ok {
		slot := pl.allocSlot()
		spec := pl.attachSpec(slot, kind, rel)
		return bound.SlotRef{Slot: slot, Name: string(kind), T: e.Type()}, []*physical.AttachSpec{spec}
	}
	switch x := e.(type) {
	case bound.Unary:
		sub, specs := pl.extractExpr(x.X)
		if len(specs) == 0 {
			return e, nil
		}
		return bound.NewUnary(x.Op, sub), specs
	case bound.Binary:
		l, ls := pl.extractExpr(x.L)
		r, rs := pl.extractExpr(x.R)
		if len(ls) == 0 && len(rs) == 0 {
			return e, nil
		}
		return bound.NewBinary(x.Op, l, r), append(ls, rs...)
	case bound.Cast:
		sub, specs := pl.extractExpr(x.X)
		if len(specs) == 0 {
			return e, nil
		}
		return bound.NewCast(sub, x.To), specs
	case bound.Branch:
		// The binder rejects crossings anywhere under a branch, eager
		// per-row attachment would evaluate crossings in never-selected
		// arms, breaking the laziness contract: so there is nothing to extract.
		return e, nil
	case bound.TextMatch:
		// The pattern is constant; only the value can hold a crossing.
		sub, specs := pl.extractExpr(x.Value)
		if len(specs) == 0 {
			return e, nil
		}
		return bound.NewTextMatch(sub, x.Pattern), specs
	}
	return e, nil
}

func (pl *planner) attachSpec(slot lir.SlotID, kind physical.CrossKind, rel bound.Relation) *physical.AttachSpec {
	return &physical.AttachSpec{
		Slot: slot,
		Kind: kind,
		Corr: analysis.Classify(rel),
		Plan: pl.plan(rel, nil),
		Out:  rel.Output(),
	}
}

// crossing destructures a crossing expression.
func crossing(e bound.Expr) (physical.CrossKind, bound.Relation, bool) {
	switch x := e.(type) {
	case bound.Exists:
		return physical.CrossExists, x.Rel, true
	case bound.First:
		return physical.CrossFirst, x.Rel, true
	case bound.Scalar:
		return physical.CrossScalar, x.Rel, true
	case bound.Array:
		return physical.CrossArray, x.Rel, true
	}
	return "", nil, false
}

// attachWrap interposes an AttachExec when there is anything to attach.
func attachWrap(input physical.PhysNode, specs []*physical.AttachSpec) physical.PhysNode {
	if len(specs) == 0 {
		return input
	}
	return &physical.AttachExec{Input: input, Specs: specs}
}

// filterChain reports whether every node between rel and its scan is a
// Filter — the shape whose conjunctions merge soundly into one residual.
func filterChain(rel bound.Relation) bool {
	for {
		switch n := rel.(type) {
		case *bound.Filter:
			rel = n.In
		case *bound.Scan:
			return true
		default:
			return false
		}
	}
}

// mergedPred conjoins a filter chain's predicates into the one full
// residual.
func mergedPred(rel bound.Relation) bound.Expr {
	var pred bound.Expr
	for {
		f, ok := rel.(*bound.Filter)
		if !ok {
			return pred
		}
		if pred == nil {
			pred = f.Pred
		} else {
			pred = bound.NewBinary(lir.OpAnd, pred, f.Pred)
		}
		rel = f.In
	}
}

// chooseAccessPath picks the cheapest access path the constraints support.
func (pl *planner) chooseAccessPath(cs *analysis.ScanConstraints, req []bound.OrderTerm) physical.PhysNode {
	scan := cs.Scan
	if pl.cfg.fullScanOnly {
		return &physical.TableScanExec{Scan: scan}
	}

	// A fully pinned primary key is a point get.
	if key, ok := pinnedKey(cs, scan.Table.PrimaryKey); ok {
		return &physical.PKGetExec{
			Scan: scan, Key: key,
			Access: &physical.AccessDecision{Candidates: []physical.AccessCandidate{{Method: "PKGet", Chosen: true}}},
		}
	}

	best := physical.PhysNode(&physical.TableScanExec{Scan: scan})
	bestScore := score(best, 0, false, req)
	cands := []physical.AccessCandidate{{Method: "TableScan", Score: bestScore}}
	chosen := 0

	for _, idx := range scan.Table.Indexes {
		if !idx.Ready() {
			continue
		}
		eq := make([]analysis.ConstVal, 0, len(idx.Columns))
		for _, col := range idx.Columns {
			d, ok := cs.Cols[col]
			if !ok || d.Eq == nil {
				break
			}
			eq = append(eq, *d.Eq)
		}
		var rng *physical.RangeSpec
		if len(eq) < len(idx.Columns) {
			col := idx.Columns[len(eq)]
			if d, ok := cs.Cols[col]; ok && (d.Lo != nil || d.Hi != nil) {
				rng = &physical.RangeSpec{Column: col, Lo: d.Lo, Hi: d.Hi}
			}
		}
		cand := &physical.IndexRangeScanExec{Scan: scan, Index: idx, EqPrefix: eq, Range: rng}
		s := score(cand, len(eq), rng != nil, req)
		cands = append(cands, physical.AccessCandidate{Method: "IndexRangeScan " + idx.Name, Score: s})
		if s > bestScore {
			best, bestScore, chosen = cand, s, len(cands)-1
		}
	}

	cands[chosen].Chosen = true
	dec := &physical.AccessDecision{Candidates: cands}
	switch n := best.(type) {
	case *physical.TableScanExec:
		n.Access = dec
	case *physical.IndexRangeScanExec:
		n.Access = dec
	}
	return best
}

// score ranks an access candidate lexicographically: equality-prefix
// length, then a trailing range, then required-ordering satisfaction.
func score(n physical.PhysNode, eqLen int, hasRange bool, req []bound.OrderTerm) int {
	s := eqLen << 2
	if hasRange {
		s |= 2
	}
	if len(req) > 0 && satisfiesOrder(n, req) {
		s |= 1
	}
	return s
}

// pinnedKey collects the equality constants covering cols, in order.
func pinnedKey(cs *analysis.ScanConstraints, cols []string) ([]analysis.ConstVal, bool) {
	if len(cols) == 0 {
		return nil, false
	}
	key := make([]analysis.ConstVal, len(cols))
	for i, col := range cols {
		d, ok := cs.Cols[col]
		if !ok || d.Eq == nil {
			return nil, false
		}
		key[i] = *d.Eq
	}
	return key, true
}
