package exec

// The reference interpreter: the semantic oracle. It evaluates bound LIR the
// most naive way that could possibly be right — full table scans, nested
// loops, per-row crossing evaluation, materialise everything — with no
// planner, no extraction, no attach machinery, and no batching. Every real
// execution must agree with it: where the conformance suite proves the
// engine equivalent to *itself* under different physical choices, the
// oracle pins what the answer actually is.

import (
	"context"
	"fmt"
	"reflect"
	"slices"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

func TestReferenceInterpreter(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	queries := conformanceQueries()
	queries["wrapped crossing arithmetic"] = many(lir.Project{
		Input: qscan("boards", "b"),
		Fields: []lir.ProjField{
			{As: "id", Expr: qcol("b", "id")},
			{As: "n", Expr: lir.Binary{Op: lir.OpAdd, L: countTasks("b"), R: qlit(1)}},
		},
	})
	queries["left join"] = many(lir.Order{
		Input: lir.Join{
			Left:  qscan("tasks", "t"),
			Right: qscan("users", "u"),
			Kind:  lir.LeftJoin,
			On:    qeq(qcol("t", "assignee_id"), qcol("u", "id")),
		},
		Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
	})

	for name, q := range queries {
		t.Run(name, func(t *testing.T) {
			engine, err := eng.Execute(ctx, q)
			if err != nil {
				t.Fatal(err)
			}
			oracle, err := interpQuery(ctx, eng, q)
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(jsonish(engine), jsonish(oracle)) {
				t.Fatalf("engine disagrees with the oracle.\n engine: %#v\n oracle: %#v",
					jsonish(engine), jsonish(oracle))
			}
		})
	}
}

// interpQuery binds q and evaluates it with the interpreter.
func interpQuery(ctx context.Context, e *Engine, q lir.Query) (lir.Datum, error) {
	bq, err := planner.Bind(ctx, e.cat, q)
	if err != nil {
		return lir.Datum{}, err
	}
	in := &interp{ctx: ctx, view: e.store, next: bq.Slots}
	rows, err := in.rel(bq.Root, bound.Env{})
	if err != nil {
		return lir.Datum{}, err
	}

	out := bq.Root.Output()
	switch bq.Card {
	case lir.CardFirst:
		if len(rows) == 0 {
			return lir.NullDatum(), nil
		}
		return frameToObject(out, rows[0]), nil
	case lir.CardExactlyOne:
		if len(rows) != 1 {
			return lir.Datum{}, fmt.Errorf("exec: expected exactly one row, got %d", len(rows))
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
	ctx  context.Context
	view kv.KV
	next lir.SlotID // scratch slots for crossing substitution
}

func (in *interp) rel(r bound.Relation, outer bound.Env) ([]bound.Env, error) {
	switch n := r.(type) {
	case *bound.Scan:
		it, err := scanTable(in.ctx, in.view, n.Table)
		if err != nil {
			return nil, err
		}
		defer it.Close()
		var out []bound.Env
		for {
			row, ok, err := it.Next()
			if err != nil {
				return nil, err
			}
			if !ok {
				return out, nil
			}
			env := newFrame(outer)
			for _, f := range n.Output().Fields {
				env.SetScalar(f.Slot, row[f.Name])
			}
			out = append(out, env)
		}

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
			for _, r := range right {
				merged := mergeFrames(l, r)
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
	return nil, fmt.Errorf("oracle: unknown relation %T", r)
}

// fold is an independent implementation of the aggregation semantics:
// grouping by encoded key in first-seen order, folds skipping NULLs, count 0
// and everything else NULL over the empty set, avg always float64.
func (in *interp) fold(n *bound.Aggregate, rows []bound.Env, outer bound.Env) ([]bound.Env, error) {
	type group struct {
		vals [][]lir.Value // arg values per term, non-NULL only
		nums []int64       // rows seen, for count(*)
		gv   []lir.Value
	}
	groups := map[string]*group{}
	var order []string

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
		key, err := EncodeTuple(gv)
		if err != nil {
			return nil, err
		}
		grp, ok := groups[string(key)]
		if !ok {
			grp = &group{vals: make([][]lir.Value, len(n.Terms)), nums: make([]int64, len(n.Terms)), gv: gv}
			groups[string(key)] = grp
			order = append(order, string(key))
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
	if len(n.Groups) == 0 && len(order) == 0 {
		groups[""] = &group{vals: make([][]lir.Value, len(n.Terms)), nums: make([]int64, len(n.Terms))}
		order = append(order, "")
	}

	var out []bound.Env
	for _, key := range order {
		grp := groups[key]
		env := newFrame(outer)
		for i, g := range n.Groups {
			env.SetScalar(g.Slot, grp.gv[i])
		}
		for i, t := range n.Terms {
			v, err := oracleFold(t, grp.vals[i], grp.nums[i])
			if err != nil {
				return nil, err
			}
			env.SetScalar(t.Slot, v)
		}
		out = append(out, env)
	}
	return out, nil
}

func oracleFold(t bound.AggTerm, vals []lir.Value, rows int64) (lir.Value, error) {
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
		var sumI int64
		var sumF float64
		for _, v := range vals {
			if v.Type == "int64" {
				sumI += v.Int64
				sumF += float64(v.Int64)
			} else {
				sumF += v.Float64
			}
		}
		if t.Fn == lir.AggAvg {
			return lir.Float64(sumF / float64(len(vals))), nil
		}
		if t.T.Kind == lir.KindInt64 {
			return lir.Int64(sumI), nil
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
	return lir.Value{}, fmt.Errorf("oracle: unknown fold %q", t.Fn)
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

// substitute replaces every crossing with a scratch slot holding its
// per-row, fully nested evaluation — the naive semantics extraction must
// preserve.
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
	return bound.SlotRef{Slot: slot, Name: "oracle", T: e.Type()}
}
