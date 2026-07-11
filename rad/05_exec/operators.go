package exec

// Operators for the relation-graph executor: one per physical node. The
// executor builds a tree bound to a KV view and an outer environment
// (correlated sub-plans see their enclosing rows' slots); scans stream,
// sort/aggregate buffer, slices stop pulling.

import (
	"context"
	"fmt"
	"slices"

	kv "rad/rad/01_kv"
	lir "rad/rad/03_lir"
	"rad/rad/03_lir/bound"
	planner "rad/rad/04_planner"
)

// executor builds operator trees. forceNested disables keyed batching —
// the equivalence property the conformance suite asserts.
type executor struct {
	view        kv.KV
	forceNested bool
	plans       map[bound.Relation]planner.PhysNode // nested-crossing plan cache
}

func newExecutor(view kv.KV) *executor {
	return &executor{view: view, plans: map[bound.Relation]planner.PhysNode{}}
}

func (ex *executor) build(ctx context.Context, node planner.PhysNode, outer bound.Env) (Operator, error) {
	switch n := node.(type) {
	case *planner.PKGetExec:
		return ex.buildPKGet(ctx, n, outer)
	case *planner.TableScanExec:
		it, err := scanTable(ctx, ex.view, n.Scan.Table)
		if err != nil {
			return nil, err
		}
		return &rowIterOp{scan: n.Scan, it: it, outer: outer}, nil
	case *planner.IndexRangeScanExec:
		return ex.buildIndexScan(ctx, n, outer)
	case *planner.FilterExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &filterOp{in: in, pred: n.Pred, glue: ex.glue(outer)}, nil
	case *planner.ProjectExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &projectOp{ex: ex, in: in, fields: n.Fields, outer: outer}, nil
	case *planner.SortExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &sortOp{ex: ex, in: in, terms: n.Terms, outer: outer}, nil
	case *planner.SliceExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &sliceOp{in: in, offset: n.Offset, limit: n.Limit}, nil
	case *planner.AggregateExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &aggOp{ex: ex, in: in, groups: n.Groups, terms: n.Terms, outer: outer}, nil
	case *planner.NestedLoopJoinExec:
		l, err := ex.build(ctx, n.L, outer)
		if err != nil {
			return nil, err
		}
		r, err := ex.build(ctx, n.R, outer)
		if err != nil {
			l.Close()
			return nil, err
		}
		return &nljOp{ex: ex, l: l, r: r, kind: n.Kind, on: n.On, rOut: n.ROut, outer: outer}, nil
	}
	return nil, fmt.Errorf("exec: unknown physical node %T", node)
}

// resolveConst turns a planner constant — literal or outer-slot parameter —
// into a value under the environment.
func resolveConst(cv planner.ConstVal, outer bound.Env) (lir.Value, error) {
	if cv.Lit != nil {
		return *cv.Lit, nil
	}
	v, ok := outer[*cv.Outer]
	if !ok {
		return lir.Value{}, fmt.Errorf("exec: outer slot %d not bound", *cv.Outer)
	}
	return v, nil
}

// rowToFrame lifts a stored row into a frame under the scan's slots.
func rowToFrame(scan *bound.Scan, row lir.Row, outer bound.Env) Frame {
	f := newFrame(outer)
	for _, fld := range scan.Output().Fields {
		f.vals[fld.Slot] = row[fld.Name]
	}
	return f
}

// ── point get ───────────────────────────────────────────────────────────────

type pkGetOp struct {
	scan  *bound.Scan
	row   lir.Row // nil when absent or a key component was NULL
	outer bound.Env
	done  bool
}

func (ex *executor) buildPKGet(ctx context.Context, n *planner.PKGetExec, outer bound.Env) (Operator, error) {
	key := lir.Row{}
	for i, col := range n.Scan.Table.PrimaryKey {
		v, err := resolveConst(n.Key[i], outer)
		if err != nil {
			return nil, err
		}
		if v.Null {
			// Equality with NULL never matches: no row, no KV op.
			return &pkGetOp{scan: n.Scan, outer: outer}, nil
		}
		key[col] = v
	}
	row, _, err := loadByPK(ctx, ex.view, n.Scan.Table, key)
	if err != nil {
		return nil, err
	}
	return &pkGetOp{scan: n.Scan, row: row, outer: outer}, nil
}

func (o *pkGetOp) Next(context.Context) (Frame, bool, error) {
	if o.done || o.row == nil {
		return Frame{}, false, nil
	}
	o.done = true
	return rowToFrame(o.scan, o.row, o.outer), true, nil
}

func (o *pkGetOp) Close() error { return nil }

// ── scans ───────────────────────────────────────────────────────────────────

// rowIterOp adapts a storage RowIterator (table or index range) to frames.
type rowIterOp struct {
	scan  *bound.Scan
	it    RowIterator
	outer bound.Env
}

func (o *rowIterOp) Next(context.Context) (Frame, bool, error) {
	row, ok, err := o.it.Next()
	if err != nil || !ok {
		return Frame{}, false, err
	}
	return rowToFrame(o.scan, row, o.outer), true, nil
}

func (o *rowIterOp) Close() error { return o.it.Close() }

func (ex *executor) buildIndexScan(ctx context.Context, n *planner.IndexRangeScanExec, outer bound.Env) (Operator, error) {
	eq := make([]lir.Value, len(n.EqPrefix))
	for i, cv := range n.EqPrefix {
		v, err := resolveConst(cv, outer)
		if err != nil {
			return nil, err
		}
		if v.Null {
			return &emptyOp{}, nil // eq NULL matches nothing
		}
		eq[i] = v
	}
	var rng *rangeBounds
	if n.Range != nil {
		rng = &rangeBounds{}
		if n.Range.Lo != nil {
			v := n.Range.Lo.V
			rng.lo, rng.loIncl = &v, n.Range.Lo.Inclusive
		}
		if n.Range.Hi != nil {
			v := n.Range.Hi.V
			rng.hi, rng.hiIncl = &v, n.Range.Hi.Inclusive
		}
	}
	it, err := scanIndexRange(ctx, ex.view, n.Scan.Table, n.Index, eq, rng)
	if err != nil {
		return nil, err
	}
	return &rowIterOp{scan: n.Scan, it: it, outer: outer}, nil
}

type emptyOp struct{}

func (emptyOp) Next(context.Context) (Frame, bool, error) { return Frame{}, false, nil }
func (emptyOp) Close() error                              { return nil }

// ── filter ──────────────────────────────────────────────────────────────────

type filterOp struct {
	in   Operator
	pred bound.Expr
	glue bound.RelEvaluator
}

func (o *filterOp) Next(ctx context.Context) (Frame, bool, error) {
	for {
		f, ok, err := o.in.Next(ctx)
		if err != nil || !ok {
			return Frame{}, false, err
		}
		t, err := bound.EvalPred(ctx, o.pred, f.vals, o.glue)
		if err != nil {
			return Frame{}, false, err
		}
		if t == lir.TriTrue {
			return f, true, nil
		}
	}
}

func (o *filterOp) Close() error { return o.in.Close() }

// ── sort ────────────────────────────────────────────────────────────────────

type sortOp struct {
	ex     *executor
	in     Operator
	terms  []bound.OrderTerm
	outer  bound.Env
	sorted []Frame
	pos    int
	primed bool
}

func (o *sortOp) Next(ctx context.Context) (Frame, bool, error) {
	if !o.primed {
		frames, err := drainOp(ctx, o.in)
		if err != nil {
			return Frame{}, false, err
		}
		keys := make([][]lir.Value, len(frames))
		glue := o.ex.glue(o.outer)
		for i, f := range frames {
			keys[i] = make([]lir.Value, len(o.terms))
			for j, t := range o.terms {
				v, err := bound.Eval(ctx, t.Expr, f.vals, glue)
				if err != nil {
					return Frame{}, false, err
				}
				keys[i][j] = v
			}
		}
		idx := make([]int, len(frames))
		for i := range idx {
			idx[i] = i
		}
		var sortErr error
		slices.SortStableFunc(idx, func(a, b int) int {
			for j, t := range o.terms {
				c, err := keys[a][j].Compare(keys[b][j])
				if err != nil && sortErr == nil {
					sortErr = err
				}
				if t.Desc {
					c = -c
				}
				if c != 0 {
					return c
				}
			}
			return 0
		})
		if sortErr != nil {
			return Frame{}, false, fmt.Errorf("exec: %w", sortErr)
		}
		o.sorted = make([]Frame, len(frames))
		for i, j := range idx {
			o.sorted[i] = frames[j]
		}
		o.primed = true
	}
	if o.pos >= len(o.sorted) {
		return Frame{}, false, nil
	}
	f := o.sorted[o.pos]
	o.pos++
	return f, true, nil
}

func (o *sortOp) Close() error { return o.in.Close() }

// ── slice ───────────────────────────────────────────────────────────────────

type sliceOp struct {
	in      Operator
	offset  int
	limit   *int
	skipped int
	emitted int
}

func (o *sliceOp) Next(ctx context.Context) (Frame, bool, error) {
	if o.limit != nil && o.emitted >= *o.limit {
		return Frame{}, false, nil // stop pulling: this is stop-early
	}
	for o.skipped < o.offset {
		_, ok, err := o.in.Next(ctx)
		if err != nil || !ok {
			return Frame{}, false, err
		}
		o.skipped++
	}
	f, ok, err := o.in.Next(ctx)
	if err != nil || !ok {
		return Frame{}, false, err
	}
	o.emitted++
	return f, true, nil
}

func (o *sliceOp) Close() error { return o.in.Close() }

// ── aggregate ───────────────────────────────────────────────────────────────

type aggOp struct {
	ex     *executor
	in     Operator
	groups []bound.GroupTerm
	terms  []bound.AggTerm
	outer  bound.Env
	out    []Frame
	pos    int
	primed bool
}

type aggAccum struct {
	count int64
	sumI  int64
	sumF  float64
	n     int64 // non-NULL inputs seen by sum/avg/min/max
	min   lir.Value
	max   lir.Value
}

func (o *aggOp) Next(ctx context.Context) (Frame, bool, error) {
	if !o.primed {
		if err := o.fold(ctx); err != nil {
			return Frame{}, false, err
		}
		o.primed = true
	}
	if o.pos >= len(o.out) {
		return Frame{}, false, nil
	}
	f := o.out[o.pos]
	o.pos++
	return f, true, nil
}

func (o *aggOp) fold(ctx context.Context) error {
	glue := o.ex.glue(o.outer)

	type group struct {
		vals  []lir.Value
		accum []aggAccum
	}
	groups := map[string]*group{}
	var order []string // deterministic output: first-seen group order

	for {
		f, ok, err := o.in.Next(ctx)
		if err != nil {
			return err
		}
		if !ok {
			break
		}
		gvals := make([]lir.Value, len(o.groups))
		for i, g := range o.groups {
			v, err := bound.Eval(ctx, g.Expr, f.vals, glue)
			if err != nil {
				return err
			}
			gvals[i] = v
		}
		key, err := EncodeTuple(gvals)
		if err != nil {
			return err
		}
		grp, ok := groups[string(key)]
		if !ok {
			grp = &group{vals: gvals, accum: make([]aggAccum, len(o.terms))}
			groups[string(key)] = grp
			order = append(order, string(key))
		}
		for i, t := range o.terms {
			a := &grp.accum[i]
			if t.Arg == nil { // count(*)
				a.count++
				continue
			}
			v, err := bound.Eval(ctx, t.Arg, f.vals, glue)
			if err != nil {
				return err
			}
			if v.Null {
				continue // folds skip NULLs
			}
			a.count++
			switch t.Fn {
			case lir.AggSum, lir.AggAvg:
				a.n++
				if v.Type == "int64" {
					a.sumI += v.Int64
					a.sumF += float64(v.Int64)
				} else {
					a.sumF += v.Float64
				}
			case lir.AggMin, lir.AggMax:
				if a.n == 0 {
					a.min, a.max = v, v
				} else {
					if c, _ := v.Compare(a.min); c < 0 {
						a.min = v
					}
					if c, _ := v.Compare(a.max); c > 0 {
						a.max = v
					}
				}
				a.n++
			}
		}
	}

	// A global fold yields exactly one row, even over nothing.
	if len(o.groups) == 0 && len(order) == 0 {
		g := &group{accum: make([]aggAccum, len(o.terms))}
		groups[""] = g
		order = append(order, "")
	}

	for _, key := range order {
		grp := groups[key]
		f := newFrame(o.outer)
		for i, g := range o.groups {
			f.vals[g.Slot] = grp.vals[i]
		}
		for i, t := range o.terms {
			f.vals[t.Slot] = foldResult(t, grp.accum[i])
		}
		o.out = append(o.out, f)
	}
	return nil
}

// foldResult applies the empty-set and typing rules: count is 0, never
// NULL; sum keeps the argument's type; avg is always float64; min/max keep
// the argument's type; every fold but count is NULL over no non-NULL input.
func foldResult(t bound.AggTerm, a aggAccum) lir.Value {
	switch t.Fn {
	case lir.AggCount:
		return lir.Int64(a.count)
	case lir.AggSum:
		if a.n == 0 {
			return lir.Null(t.T.Kind.CatalogType())
		}
		if t.T.Kind == lir.KindInt64 {
			return lir.Int64(a.sumI)
		}
		return lir.Float64(a.sumF)
	case lir.AggAvg:
		if a.n == 0 {
			return lir.Null(t.T.Kind.CatalogType())
		}
		return lir.Float64(a.sumF / float64(a.n))
	case lir.AggMin:
		if a.n == 0 {
			return lir.Null(t.T.Kind.CatalogType())
		}
		return a.min
	default: // max
		if a.n == 0 {
			return lir.Null(t.T.Kind.CatalogType())
		}
		return a.max
	}
}

func (o *aggOp) Close() error { return o.in.Close() }

// ── nested-loop join ────────────────────────────────────────────────────────

type nljOp struct {
	ex    *executor
	l, r  Operator
	kind  lir.JoinKind
	on    bound.Expr
	rOut  lir.RowType
	outer bound.Env

	right   []Frame
	cur     *Frame
	rPos    int
	matched bool
	primed  bool
}

func (o *nljOp) Next(ctx context.Context) (Frame, bool, error) {
	if !o.primed {
		right, err := drainOp(ctx, o.r)
		if err != nil {
			return Frame{}, false, err
		}
		o.right = right
		o.primed = true
	}
	glue := o.ex.glue(o.outer)
	for {
		if o.cur == nil {
			f, ok, err := o.l.Next(ctx)
			if err != nil || !ok {
				return Frame{}, false, err
			}
			o.cur, o.rPos, o.matched = &f, 0, false
		}
		for o.rPos < len(o.right) {
			r := o.right[o.rPos]
			o.rPos++
			merged := mergeFrames(*o.cur, r)
			t, err := bound.EvalPred(ctx, o.on, merged.vals, glue)
			if err != nil {
				return Frame{}, false, err
			}
			if t == lir.TriTrue {
				o.matched = true
				return merged, true, nil
			}
		}
		left := *o.cur
		o.cur = nil
		if o.kind == lir.LeftJoin && !o.matched {
			return o.nullPadded(left), true, nil
		}
	}
}

// mergeFrames unions two frames — slots are disjoint by construction.
func mergeFrames(l, r Frame) Frame {
	out := Frame{vals: make(bound.Env, len(l.vals)+len(r.vals))}
	for k, v := range l.vals {
		out.vals[k] = v
	}
	for k, v := range r.vals {
		out.vals[k] = v
	}
	for k, v := range l.nested {
		out = out.setNested(k, v)
	}
	for k, v := range r.nested {
		out = out.setNested(k, v)
	}
	return out
}

// nullPadded extends an unmatched left row with typed NULLs for the right
// side's output.
func (o *nljOp) nullPadded(left Frame) Frame {
	out := mergeFrames(left, Frame{})
	for _, f := range o.rOut.Fields {
		if f.Type.Kind.Scalar() {
			out.vals[f.Slot] = lir.Null(f.Type.Kind.CatalogType())
		}
	}
	return out
}

func (o *nljOp) Close() error {
	err := o.l.Close()
	if e := o.r.Close(); err == nil {
		err = e
	}
	return err
}
