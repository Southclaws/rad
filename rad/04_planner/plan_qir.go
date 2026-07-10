package planner

// Physical planning: bound IR to the operator tree. Pure — everything was
// resolved and validated by the binder, so planning cannot fail on client
// input; it only chooses implementations.
//
// Access selection generalises v1's equality-only chooseAccessPath: a fully
// pinned primary key wins outright; otherwise indexes rank by leading
// equality-prefix length, then a trailing range bound, then whether the
// index's provided ordering satisfies the required ordering (the whole of
// the limited ordered-index pushdown policy); otherwise a table scan. The
// full predicate always rides above the access node.

import (
	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
)

// PlanQuery lowers a bound query into a physical plan.
func PlanQuery(q *bound.Query) *PhysPlan {
	return &PhysPlan{
		Root: plan(q.Root, nil),
		Card: q.Card,
		Out:  q.Root.Output(),
	}
}

// plan lowers one relation. req is the ordering an enclosing Order needs —
// flowing it down lets access selection prefer an order-providing index, so
// the Order above can disappear entirely.
func plan(rel bound.Relation, req []bound.OrderTerm) PhysNode {
	switch n := rel.(type) {
	case *bound.Scan:
		return chooseAccessPath(&ScanConstraints{Scan: n, Cols: map[string]Domain{}}, req)

	case *bound.Filter:
		// A filter chain over a scan plans as a unit: constraints choose the
		// access path, and the merged conjunction rides above it.
		if scan := underlyingScan(n); scan != nil && filterChain(n) {
			cs := ExtractConstraints(n)
			access := chooseAccessPath(cs, req)
			return &FilterExec{Input: access, Pred: mergedPred(n)}
		}
		return &FilterExec{Input: plan(n.In, req), Pred: n.Pred}

	case *bound.Order:
		in := plan(n.In, n.Terms)
		if satisfiesOrder(in, n.Terms) {
			return in
		}
		return &SortExec{Input: in, Terms: n.Terms}

	case *bound.Slice:
		// Ordering does not commute with slicing, so nothing flows through.
		return &SliceExec{Input: plan(n.In, nil), Offset: n.Offset, Limit: n.Limit}

	case *bound.Project:
		fields := make([]PhysField, len(n.Fields))
		for i, f := range n.Fields {
			fields[i] = planField(f)
		}
		return &ProjectExec{Input: plan(n.In, nil), Fields: fields}

	case *bound.Aggregate:
		return &AggregateExec{Input: plan(n.In, nil), Groups: n.Groups, Terms: n.Terms}

	case *bound.Join:
		return &NestedLoopJoinExec{L: plan(n.L, nil), R: plan(n.R, nil), Kind: n.Kind, On: n.On}
	}
	panic("planner: unplannable relation") // sealed interface; unreachable
}

// planField lowers one projection field. Crossings become attached
// sub-plans; anything else stays a scalar expression.
func planField(f bound.ProjField) PhysField {
	out := PhysField{Name: f.Name, Slot: f.Slot}
	var kind CrossKind
	var rel bound.Relation
	switch x := f.Expr.(type) {
	case bound.Exists:
		kind, rel = CrossExists, x.Rel
	case bound.First:
		kind, rel = CrossFirst, x.Rel
	case bound.Scalar:
		kind, rel = CrossScalar, x.Rel
	case bound.Array:
		kind, rel = CrossArray, x.Rel
	default:
		out.Expr = f.Expr
		return out
	}
	out.Attach = &AttachSpec{
		Kind: kind,
		Corr: Classify(rel),
		Plan: plan(rel, nil),
		Out:  rel.Output(),
	}
	return out
}

// PlanRelation lowers a bare relation — the executor's nested-evaluation
// glue plans crossing sub-relations on demand with it.
func PlanRelation(rel bound.Relation) PhysNode { return plan(rel, nil) }

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
			pred = bound.NewBinary(qir.OpAnd, pred, f.Pred)
		}
		rel = f.In
	}
}

// chooseAccessPath picks the cheapest access path the constraints support.
func chooseAccessPath(cs *ScanConstraints, req []bound.OrderTerm) PhysNode {
	scan := cs.Scan

	// A fully pinned primary key is a point get.
	if key, ok := pinnedKey(cs, scan.Table.PrimaryKey); ok {
		return &PKGetExec{Scan: scan, Key: key}
	}

	best := PhysNode(&TableScanExec{Scan: scan})
	bestScore := score(best, 0, false, req)

	for _, idx := range scan.Table.Indexes {
		eq := make([]ConstVal, 0, len(idx.Columns))
		for _, col := range idx.Columns {
			d, ok := cs.Cols[col]
			if !ok || d.Eq == nil {
				break
			}
			eq = append(eq, *d.Eq)
		}
		var rng *RangeSpec
		if len(eq) < len(idx.Columns) {
			col := idx.Columns[len(eq)]
			if d, ok := cs.Cols[col]; ok && (d.Lo != nil || d.Hi != nil) {
				rng = &RangeSpec{Column: col, Lo: d.Lo, Hi: d.Hi}
			}
		}
		cand := &IndexRangeScanExec{Scan: scan, Index: idx, EqPrefix: eq, Range: rng}
		if s := score(cand, len(eq), rng != nil, req); s > bestScore {
			best, bestScore = cand, s
		}
	}
	return best
}

// score ranks an access candidate lexicographically: equality-prefix
// length, then a trailing range, then required-ordering satisfaction.
func score(n PhysNode, eqLen int, hasRange bool, req []bound.OrderTerm) int {
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
func pinnedKey(cs *ScanConstraints, cols []string) ([]ConstVal, bool) {
	if len(cols) == 0 {
		return nil, false
	}
	key := make([]ConstVal, len(cols))
	for i, col := range cols {
		d, ok := cs.Cols[col]
		if !ok || d.Eq == nil {
			return nil, false
		}
		key[i] = *d.Eq
	}
	return key, true
}
