// Package refexec is the reference interpreter for bound LIR: the semantic
// oracle. It evaluates a *bound.Query the most naive way that could possibly be
// right — materialise every relation, nested-loop joins, per-row crossing
// evaluation, linear grouping, in-memory sort — with no planner, no physical
// plan, no index scans, no attach machinery, and no batching. Every real
// execution must agree with it: where path-independence proves the engine
// equivalent to itself under different physical choices, this pins what the
// answer actually IS.
//
// It is deliberately, structurally independent of the executor it validates:
//
//   - It shares NO query logic with package exec (rad/engine/05_exec). It is
//     nested there only for layer organisation; it imports none of exec's
//     scan/planner/operator/attach/index code. It reimplements the relational
//     operators itself, here.
//   - Stored rows enter through an injected ScanFunc, so the interpreter
//     depends on no storage implementation — the harness can feed it a pure
//     in-memory table map or the rows a real store returned.
//   - The one deliberate sharing is scalar expression evaluation
//     (bound.EvalPred / bound.EvalDatum): 3VL, arithmetic, and casts. That is
//     acceptable only because those scalar semantics are pinned independently
//     by enumerated truth-table/edge-value tests.
//
// Design law: if a line in here is clever, it is wrong. Slow is fine; obvious
// is the point. It must be so simple it needs no tests of its own.
package refexec

import (
	"context"
	"fmt"
	"math/big"
	"slices"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// ScanFunc yields all stored rows of a table, materialised, from the caller's
// snapshot. The caller supplies it; refexec never reads storage itself.
type ScanFunc func(ctx context.Context, tbl catalog.Table) ([]lir.Row, error)

// Interpret evaluates an already-bound query the dumbest correct way and
// returns the same lir.Datum shape the real engine produces. Binding is shared
// with production on purpose — it is deterministic name resolution and slot
// allocation, not the interesting semantics. The split is after binding: this
// is the oracle for planner + physical plan + execution.
func Interpret(ctx context.Context, scan ScanFunc, q *bound.Query) (lir.Datum, error) {
	in := &interp{ctx: ctx, scan: scan, next: q.Slots, bindings: map[string][]bound.Env{}, frontier: map[string][]bound.Env{}}

	// Commit-once, exactly like the engine: each binding evaluates once, in
	// dependency order, and every ref observes that one committed value. A
	// recursive binding commits its fixpoint instead of a single evaluation.
	for _, bnd := range q.Bindings {
		var committed []bound.Env
		var err error
		if bnd.Recursive {
			committed, err = in.fixpoint(bnd)
		} else {
			committed, err = in.rel(bnd.Root, bound.Env{})
		}
		if err != nil {
			return lir.Datum{}, err
		}
		in.bindings[bnd.Name] = committed
	}

	rows, err := in.rel(q.Root, bound.Env{})
	if err != nil {
		return lir.Datum{}, err
	}

	out := q.Root.Output()
	switch q.Card {
	case lir.CardFirst:
		if len(rows) == 0 {
			return lir.NullDatum(), nil
		}
		return frameToObject(out, rows[0]), nil
	case lir.CardExactlyOne:
		if len(rows) != 1 {
			return lir.Datum{}, fmt.Errorf("refexec: expected exactly one row, got %d", len(rows))
		}
		return frameToObject(out, rows[0]), nil
	case lir.CardScalar:
		if len(rows) == 0 {
			return lir.NullDatum(), nil
		}
		if d, ok := rows[0][out.Fields[0].Slot]; ok {
			return d, nil
		}
		return lir.NullDatum(), nil
	default:
		elems := make([]lir.Datum, len(rows))
		for i, r := range rows {
			elems[i] = frameToObject(out, r)
		}
		return lir.ArrayDatum(elems), nil
	}
}

type interp struct {
	ctx      context.Context
	scan     ScanFunc
	next     lir.SlotID // scratch slots for crossing substitution
	bindings map[string][]bound.Env
	// frontier holds the current working table of each in-progress recursive
	// binding, read by its step's recursive_ref.
	frontier map[string][]bound.Env
}

// newFrame starts a frame seeded from the outer environment — a correlated
// sub-relation sees its enclosing rows' slots exactly like its own.
func newFrame(outer bound.Env) bound.Env {
	f := make(bound.Env, len(outer)+8)
	for k, v := range outer {
		f[k] = v
	}
	return f
}

// mergeFrames unions two frames; slots are disjoint by construction.
func mergeFrames(l, r bound.Env) bound.Env {
	out := make(bound.Env, len(l)+len(r))
	for k, v := range l {
		out[k] = v
	}
	for k, v := range r {
		out[k] = v
	}
	return out
}

// frameToObject renders a frame as an object datum in the row type's field
// order; a missing slot is NULL. This must match the engine's rendering
// exactly, or the differential would fire on shape, not semantics.
func frameToObject(out lir.RowType, f bound.Env) lir.Datum {
	fields := make([]lir.ObjectField, len(out.Fields))
	for i, fld := range out.Fields {
		d, ok := f[fld.Slot]
		if !ok {
			d = lir.NullDatum()
		}
		fields[i] = lir.ObjectField{Name: fld.Name, Datum: d}
	}
	return lir.ObjectDatum(fields)
}

// Recursion safeguards: a valid recursive query can still fail to terminate,
// so the fixpoint is bounded. These are backstops, not semantics.
const (
	maxRecursionIterations = 10000
	maxRecursionRows       = 1_000_000
)

// slotProjection maps a source slot onto a canonical output slot.
type slotProjection struct{ src, dst lir.SlotID }

// makeProjection precomputes how a produced row (the source's fields) maps onto
// the canonical output slots, matched by name. A canonical field with no source
// is a broken bound-plan invariant, so it errors rather than silently reading
// slot zero.
func makeProjection(canon, source []lir.Field) ([]slotProjection, error) {
	byName := make(map[string]lir.SlotID, len(source))
	for _, f := range source {
		byName[f.Name] = f.Slot
	}
	pairs := make([]slotProjection, 0, len(canon))
	for _, cf := range canon {
		src, ok := byName[cf.Name]
		if !ok {
			return nil, fmt.Errorf("refexec: recursive output missing canonical field %q", cf.Name)
		}
		pairs = append(pairs, slotProjection{src: src, dst: cf.Slot})
	}
	return pairs, nil
}

// project rebuilds a row on the canonical slots. A missing source slot is a
// broken invariant, never silently dropped, so a "missing" cell can never
// masquerade as a NULL in a downstream identity test.
func project(row bound.Env, pairs []slotProjection) (bound.Env, error) {
	out := make(bound.Env, len(pairs))
	for _, p := range pairs {
		d, ok := row[p.src]
		if !ok {
			return nil, fmt.Errorf("refexec: recursive output row missing source slot %d", p.src)
		}
		out[p.dst] = d
	}
	return out, nil
}

// fixpoint evaluates a recursive binding by semi-naive iteration: the anchor
// seeds the result and the frontier; each round the step runs with the frontier
// in scope, its rows — admitted against the whole result under admit-new
// accumulation, by refexec's own canonical identity — become the next frontier
// and extend the result, until the frontier is empty. Rows are projected onto
// the binding's canonical slots so every occurrence — the outer ref and the
// step's recursive_ref — reads them the same way.
func (in *interp) fixpoint(bnd *bound.Binding) ([]bound.Env, error) {
	canon := bnd.Root.Output().Fields
	anchorProj, err := makeProjection(canon, canon)
	if err != nil {
		return nil, err
	}
	stepProj, err := makeProjection(canon, bnd.Step.Output().Fields)
	if err != nil {
		return nil, err
	}

	var seen *oracleRowSet
	if bnd.Accumulation == lir.AccumulateNew {
		seen = newOracleRowSet(canon)
	}
	admit := func(row bound.Env) (bound.Env, bool) {
		if seen == nil {
			return row, true
		}
		if !seen.add(row) {
			return nil, false
		}
		return row, true
	}

	// The frontier map is shared interpreter state; restore it on every exit —
	// success, cap, or a step error — so a failed query leaves nothing stale and
	// a re-entrant evaluation would stay correct.
	prev, existed := in.frontier[bnd.Name]
	defer func() {
		if existed {
			in.frontier[bnd.Name] = prev
		} else {
			delete(in.frontier, bnd.Name)
		}
	}()

	overRowCap := func() error {
		return reject.Fail(reject.ReasonRecursionLimit, "refexec: recursive binding %q produced more than %d rows", bnd.Name, maxRecursionRows)
	}

	anchorRows, err := in.rel(bnd.Root, bound.Env{})
	if err != nil {
		return nil, err
	}
	var result, frontier []bound.Env
	for _, row := range anchorRows {
		c, err := project(row, anchorProj)
		if err != nil {
			return nil, err
		}
		if adm, ok := admit(c); ok {
			result = append(result, adm)
			frontier = append(frontier, adm)
			if len(result) > maxRecursionRows {
				return nil, overRowCap()
			}
		}
	}

	for i := 0; len(frontier) > 0; i++ {
		if i >= maxRecursionIterations {
			return nil, reject.Fail(reject.ReasonRecursionLimit, "refexec: recursive binding %q did not terminate within %d iterations", bnd.Name, maxRecursionIterations)
		}
		in.frontier[bnd.Name] = frontier
		produced, err := in.rel(bnd.Step, bound.Env{})
		if err != nil {
			return nil, err
		}
		var next []bound.Env
		for _, row := range produced {
			c, err := project(row, stepProj)
			if err != nil {
				return nil, err
			}
			if adm, ok := admit(c); ok {
				next = append(next, adm)
				result = append(result, adm)
				if len(result) > maxRecursionRows {
					return nil, overRowCap()
				}
			}
		}
		frontier = next
	}
	return result, nil
}

func (in *interp) rel(r bound.Relation, outer bound.Env) ([]bound.Env, error) {
	switch n := r.(type) {
	case *bound.Ref:
		src, ok := in.bindings[n.Binding]
		if !ok {
			return nil, fmt.Errorf("refexec: binding %q not committed", n.Binding)
		}
		out := make([]bound.Env, len(src))
		for i, row := range src {
			env := newFrame(outer)
			for j, fld := range n.Output().Fields {
				if d, has := row[n.Canon[j]]; has {
					env[fld.Slot] = d
				}
			}
			out[i] = env
		}
		return out, nil

	case *bound.RecursiveRef:
		src, ok := in.frontier[n.Binding]
		if !ok {
			return nil, fmt.Errorf("refexec: frontier for %q not available", n.Binding)
		}
		out := make([]bound.Env, len(src))
		for i, row := range src {
			env := newFrame(outer)
			for j, fld := range n.Output().Fields {
				if d, has := row[n.Canon[j]]; has {
					env[fld.Slot] = d
				}
			}
			out[i] = env
		}
		return out, nil

	case *bound.Distinct:
		rows, err := in.rel(n.In, outer)
		if err != nil {
			return nil, err
		}
		seen := newOracleRowSet(n.Output().Fields)
		var deduped []bound.Env
		for _, row := range rows {
			if seen.add(row) {
				deduped = append(deduped, row)
			}
		}
		return deduped, nil

	case *bound.Rows:
		out := make([]bound.Env, len(n.Vals))
		for i, cells := range n.Vals {
			env := newFrame(outer)
			for j, f := range n.Output().Fields {
				env.SetScalar(f.Slot, cells[j])
			}
			out[i] = env
		}
		return out, nil

	case *bound.Scan:
		rows, err := in.scan(in.ctx, n.Table)
		if err != nil {
			return nil, err
		}
		out := make([]bound.Env, 0, len(rows))
		for _, row := range rows {
			env := newFrame(outer)
			for _, f := range n.Output().Fields {
				env.SetScalar(f.Slot, row[f.Name])
			}
			out = append(out, env)
		}
		return out, nil

	case *bound.Filter:
		rows, err := in.rel(n.In, outer)
		if err != nil {
			return nil, err
		}
		var out []bound.Env
		for _, env := range rows {
			tb, err := in.pred(n.Pred, env)
			if err != nil {
				return nil, err
			}
			if tb == lir.TriTrue {
				out = append(out, env)
			}
		}
		return out, nil

	case *bound.Project:
		rows, err := in.rel(n.In, outer)
		if err != nil {
			return nil, err
		}
		out := make([]bound.Env, len(rows))
		for i, env := range rows {
			proj := newFrame(outer)
			for _, f := range n.Fields {
				d, err := in.evalDatum(f.Expr, env)
				if err != nil {
					return nil, err
				}
				proj[f.Slot] = d
			}
			out[i] = proj
		}
		return out, nil

	case *bound.Join:
		left, err := in.rel(n.L, outer)
		if err != nil {
			return nil, err
		}
		right, err := in.rel(n.R, outer)
		if err != nil {
			return nil, err
		}
		var out []bound.Env
		for _, l := range left {
			matched := false
			for _, rgt := range right {
				merged := mergeFrames(l, rgt)
				tb, err := in.pred(n.On, merged)
				if err != nil {
					return nil, err
				}
				if tb == lir.TriTrue {
					matched = true
					out = append(out, merged)
				}
			}
			if !matched && n.Kind == lir.LeftJoin {
				padded := mergeFrames(l, bound.Env{})
				for _, f := range n.R.Output().Fields {
					padded[f.Slot] = lir.NullDatum()
				}
				out = append(out, padded)
			}
		}
		return out, nil

	case *bound.Order:
		rows, err := in.rel(n.In, outer)
		if err != nil {
			return nil, err
		}
		return in.order(n, rows)

	case *bound.Slice:
		rows, err := in.rel(n.In, outer)
		if err != nil {
			return nil, err
		}
		if n.Offset >= len(rows) {
			return nil, nil
		}
		rows = rows[n.Offset:]
		if n.Limit != nil && *n.Limit < len(rows) {
			rows = rows[:*n.Limit]
		}
		return rows, nil

	case *bound.Aggregate:
		rows, err := in.rel(n.In, outer)
		if err != nil {
			return nil, err
		}
		return in.fold(n, rows, outer)
	}
	return nil, fmt.Errorf("refexec: unknown relation %T", r)
}

// order sorts rows by the terms with a stable sort over explicit Value
// comparison; NULLs sort first ascending, last descending (Value.Compare's
// convention), inverted with the term.
func (in *interp) order(n *bound.Order, rows []bound.Env) ([]bound.Env, error) {
	keys := make([][]lir.Value, len(rows))
	for i, env := range rows {
		keys[i] = make([]lir.Value, len(n.Terms))
		for j, term := range n.Terms {
			d, err := in.evalDatum(term.Expr, env)
			if err != nil {
				return nil, err
			}
			if d.Kind == lir.DatumScalar {
				keys[i][j] = d.Scalar
			} else {
				keys[i][j] = lir.Value{Null: true}
			}
		}
	}
	idx := make([]int, len(rows))
	for i := range idx {
		idx[i] = i
	}
	var sortErr error
	slices.SortStableFunc(idx, func(a, b int) int {
		for j, term := range n.Terms {
			c, err := keys[a][j].Compare(keys[b][j])
			if err != nil && sortErr == nil {
				sortErr = err
			}
			if term.Desc {
				c = -c
			}
			if c != 0 {
				return c
			}
		}
		return 0
	})
	if sortErr != nil {
		return nil, sortErr
	}
	out := make([]bound.Env, len(rows))
	for i, j := range idx {
		out[i] = rows[j]
	}
	return out, nil
}

// fold is an independent implementation of aggregation: grouping by group-value
// vector in first-seen order (linear bucketing, NULL == NULL for grouping),
// folds skipping NULLs, count 0 and everything else NULL over the empty set,
// avg always float64.
func (in *interp) fold(n *bound.Aggregate, rows []bound.Env, outer bound.Env) ([]bound.Env, error) {
	type group struct {
		gv   []lir.Value   // the group key
		vals [][]lir.Value // non-NULL arg values per term
		nums []int64       // rows seen, for count(*)
	}
	var groups []*group

	find := func(gv []lir.Value) (*group, error) {
		for _, g := range groups {
			eq, err := groupEqual(g.gv, gv)
			if err != nil {
				return nil, err
			}
			if eq {
				return g, nil
			}
		}
		g := &group{gv: gv, vals: make([][]lir.Value, len(n.Terms)), nums: make([]int64, len(n.Terms))}
		groups = append(groups, g)
		return g, nil
	}

	for _, env := range rows {
		gv := make([]lir.Value, len(n.Groups))
		for i, g := range n.Groups {
			d, err := in.evalDatum(g.Expr, env)
			if err != nil {
				return nil, err
			}
			if d.Kind == lir.DatumScalar {
				gv[i] = d.Scalar
			} else {
				gv[i] = lir.Value{Null: true}
			}
		}
		grp, err := find(gv)
		if err != nil {
			return nil, err
		}
		for i, t := range n.Terms {
			grp.nums[i]++
			if t.Arg == nil {
				continue
			}
			d, err := in.evalDatum(t.Arg, env)
			if err != nil {
				return nil, err
			}
			if d.Kind == lir.DatumScalar && !d.Scalar.Null {
				grp.vals[i] = append(grp.vals[i], d.Scalar)
			}
		}
	}

	// A global fold over zero rows still yields exactly one row.
	if len(n.Groups) == 0 && len(groups) == 0 {
		groups = append(groups, &group{vals: make([][]lir.Value, len(n.Terms)), nums: make([]int64, len(n.Terms))})
	}

	var out []bound.Env
	for _, grp := range groups {
		env := newFrame(outer)
		for i, g := range n.Groups {
			env.SetScalar(g.Slot, grp.gv[i])
		}
		for i, t := range n.Terms {
			v, err := fold1(t, grp.vals[i], grp.nums[i])
			if err != nil {
				return nil, err
			}
			env.SetScalar(t.Slot, v)
		}
		out = append(out, env)
	}
	return out, nil
}

// groupEqual reports whether two group-key vectors are the same group: NULLs
// group together (NULL == NULL), non-NULLs compare by value.
func groupEqual(a, b []lir.Value) (bool, error) {
	for i := range a {
		if a[i].Null != b[i].Null {
			return false, nil
		}
		if a[i].Null {
			continue
		}
		c, err := a[i].Compare(b[i])
		if err != nil {
			return false, err
		}
		if c != 0 {
			return false, nil
		}
	}
	return true, nil
}

func fold1(t bound.AggTerm, vals []lir.Value, rows int64) (lir.Value, error) {
	switch t.Fn {
	case lir.AggCount:
		if t.Arg == nil {
			return lir.Int64(rows), nil
		}
		return lir.Int64(int64(len(vals))), nil
	}
	if len(vals) == 0 {
		return lir.Null(t.T.Kind.CatalogType()), nil
	}
	switch t.Fn {
	case lir.AggSum, lir.AggAvg:
		sumI := new(big.Int) // exact total; int64 overflow is a data error, not a wrap
		var sumF float64
		for _, v := range vals {
			if v.Type == catalog.TypeInt64 {
				sumI.Add(sumI, big.NewInt(v.Int64))
				sumF += float64(v.Int64)
			} else {
				sumF += v.Float64
			}
		}
		if t.Fn == lir.AggAvg {
			return lir.Float64(sumF / float64(len(vals))), nil
		}
		if t.T.Kind == lir.KindInt64 {
			if !sumI.IsInt64() {
				return lir.Value{}, reject.Runtimef("refexec: integer overflow in sum")
			}
			return lir.Int64(sumI.Int64()), nil
		}
		return lir.Float64(sumF), nil
	case lir.AggMin, lir.AggMax:
		best := vals[0]
		for _, v := range vals[1:] {
			c, err := v.Compare(best)
			if err != nil {
				return lir.Value{}, err
			}
			if (t.Fn == lir.AggMin && c < 0) || (t.Fn == lir.AggMax && c > 0) {
				best = v
			}
		}
		return best, nil
	}
	return lir.Value{}, fmt.Errorf("refexec: unknown fold %q", t.Fn)
}

// pred evaluates a predicate after substituting crossings per row.
func (in *interp) pred(e bound.Expr, env bound.Env) (lir.TriBool, error) {
	scratch := newFrame(env)
	e2, err := in.substitute(e, scratch)
	if err != nil {
		return lir.TriUnknown, err
	}
	return bound.EvalPred(e2, scratch)
}

// evalDatum evaluates any expression after substituting crossings per row.
func (in *interp) evalDatum(e bound.Expr, env bound.Env) (lir.Datum, error) {
	scratch := newFrame(env)
	e2, err := in.substitute(e, scratch)
	if err != nil {
		return lir.Datum{}, err
	}
	return bound.EvalDatum(e2, scratch)
}

// substitute replaces every crossing with a scratch slot holding its per-row,
// fully nested evaluation — the naive semantics the engine's extraction and
// attach machinery must preserve.
func (in *interp) substitute(e bound.Expr, env bound.Env) (bound.Expr, error) {
	switch x := e.(type) {
	case bound.Exists:
		rows, err := in.rel(x.Rel, env)
		if err != nil {
			return nil, err
		}
		return in.scratch(env, e, lir.ScalarDatum(lir.Bool(len(rows) > 0))), nil
	case bound.First:
		rows, err := in.rel(x.Rel, env)
		if err != nil {
			return nil, err
		}
		d := lir.NullDatum()
		if len(rows) > 0 {
			d = frameToObject(x.Rel.Output(), rows[0])
		}
		return in.scratch(env, e, d), nil
	case bound.Scalar:
		rows, err := in.rel(x.Rel, env)
		if err != nil {
			return nil, err
		}
		d := lir.NullDatum()
		if len(rows) > 0 {
			if v, ok := rows[0][x.Rel.Output().Fields[0].Slot]; ok {
				d = v
			}
		}
		return in.scratch(env, e, d), nil
	case bound.Array:
		rows, err := in.rel(x.Rel, env)
		if err != nil {
			return nil, err
		}
		elems := make([]lir.Datum, len(rows))
		for i, r := range rows {
			elems[i] = frameToObject(x.Rel.Output(), r)
		}
		return in.scratch(env, e, lir.ArrayDatum(elems)), nil
	case bound.Unary:
		sub, err := in.substitute(x.X, env)
		if err != nil {
			return nil, err
		}
		return bound.NewUnary(x.Op, sub), nil
	case bound.Binary:
		l, err := in.substitute(x.L, env)
		if err != nil {
			return nil, err
		}
		r, err := in.substitute(x.R, env)
		if err != nil {
			return nil, err
		}
		return bound.NewBinary(x.Op, l, r), nil
	case bound.Cast:
		sub, err := in.substitute(x.X, env)
		if err != nil {
			return nil, err
		}
		return bound.NewCast(sub, x.To), nil
	}
	return e, nil
}

func (in *interp) scratch(env bound.Env, e bound.Expr, d lir.Datum) bound.Expr {
	slot := in.next
	in.next++
	env[slot] = d
	return bound.SlotRef{Slot: slot, Name: "refexec", T: e.Type()}
}
