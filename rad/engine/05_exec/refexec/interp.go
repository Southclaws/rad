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
//     by enumerated truth-table/edge-value tests; a fully independent evaluator
//     is the eventual close-out (see the oracle ADR).
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
	in := &interp{ctx: ctx, scan: scan, next: q.Slots, bindings: map[string][]bound.Env{}}

	// Commit-once, exactly like the engine: each binding evaluates once, in
	// dependency order, and every ref observes that one committed value.
	for _, bnd := range q.Bindings {
		committed, err := in.rel(bnd.Root, bound.Env{})
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
