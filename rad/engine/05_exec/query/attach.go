package query

// attachOp evaluates extracted sub-plans without changing input order.
// Uncorrelated plans run once, key-correlated plans run once per distinct key,
// and general correlations run once per input frame.

import (
	"context"
	"fmt"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	"github.com/Southclaws/rad/rad/engine/04_planner/analysis"
	planner "github.com/Southclaws/rad/rad/engine/04_planner/physical"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

type attachOp struct {
	ex    *Executor
	in    operator
	specs []*planner.AttachSpec
	outer lireval.Env

	out    []lireval.Env
	pos    int
	primed bool
}

func (o *attachOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	if !o.primed {
		if err := o.attachAll(ctx); err != nil {
			return lireval.Env{}, false, err
		}
		o.primed = true
	}
	if o.pos >= len(o.out) {
		return lireval.Env{}, false, nil
	}
	f := o.out[o.pos]
	o.pos++
	return f, true, nil
}

func (o *attachOp) Close() error { return o.in.Close() }

func (o *attachOp) attachAll(ctx context.Context) error {
	batch, err := drainOp(ctx, o.in)
	if err != nil {
		return err
	}
	o.out = batch
	for _, spec := range o.specs {
		if err := o.attach(ctx, spec, batch); err != nil {
			return err
		}
	}
	return nil
}

func (o *attachOp) attach(ctx context.Context, a *planner.AttachSpec, batch []lireval.Env) error {
	switch {
	case a.Corr.Kind == analysis.Uncorrelated:
		d, err := o.runAttach(ctx, a, o.outer)
		if err != nil {
			return err
		}
		for i := range batch {
			batch[i][a.Slot] = d
		}
		return nil

	case a.Corr.Kind == analysis.KeyCorrelated && !o.ex.forceNested:
		// NULL keys still execute their sub-plan. Their access path may be empty,
		// but a global aggregate above that path still produces one row.
		type keyGroup struct {
			env    lireval.Env
			frames []int
		}
		groups := map[string]*keyGroup{}
		var order []string

		for i, in := range batch {
			keyVals := make([]lir.Value, len(a.Corr.Keys))
			for j, k := range a.Corr.Keys {
				v, err := in.ScalarAt(k.OuterSlot, k.InnerCol, lir.Type{})
				if err != nil {
					return err
				}
				keyVals[j] = v
			}
			enc, err := codec.EncodeTuple(keyVals)
			if err != nil {
				return err
			}
			g, ok := groups[string(enc)]
			if !ok {
				env := make(lireval.Env, len(o.outer)+len(a.Corr.Keys))
				for k, v := range o.outer {
					env[k] = v
				}
				for j, k := range a.Corr.Keys {
					env.SetScalar(k.OuterSlot, keyVals[j])
				}
				g = &keyGroup{env: env}
				groups[string(enc)] = g
				order = append(order, string(enc))
			}
			g.frames = append(g.frames, i)
		}

		for _, key := range order {
			g := groups[key]
			d, err := o.runAttach(ctx, a, g.env)
			if err != nil {
				return err
			}
			for _, i := range g.frames {
				batch[i][a.Slot] = d
			}
		}
		return nil

	default: // general correlation, or batching disabled
		for i, in := range batch {
			d, err := o.runAttach(ctx, a, in)
			if err != nil {
				return err
			}
			batch[i][a.Slot] = d
		}
		return nil
	}
}

func (o *attachOp) runAttach(ctx context.Context, a *planner.AttachSpec, env lireval.Env) (lir.Datum, error) {
	op, err := o.ex.build(ctx, a.Plan, env)
	if err != nil {
		return lir.Datum{}, err
	}
	defer op.Close()

	switch a.Kind {
	case planner.CrossExists:
		_, ok, err := op.Next(ctx)
		if err != nil {
			return lir.Datum{}, err
		}
		return lir.ScalarDatum(lir.Bool(ok)), nil

	case planner.CrossFirst:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return lir.Datum{}, err
		}
		if !ok {
			return lir.NullDatum(), nil
		}
		return frameToObject(a.Out, f), nil

	case planner.CrossScalar:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return lir.Datum{}, err
		}
		if !ok {
			return lir.NullDatum(), nil
		}
		return frameScalar(a.Out, f), nil

	case planner.CrossArray:
		frames, err := drainOp(ctx, op)
		if err != nil {
			return lir.Datum{}, err
		}
		return framesToArray(a.Out, frames), nil
	}
	return lir.Datum{}, fmt.Errorf("exec: unknown crossing %q", a.Kind)
}

type projectOp struct {
	in     operator
	fields []planner.PhysField
	outer  lireval.Env
}

func (o *projectOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	in, ok, err := o.in.Next(ctx)
	if err != nil || !ok {
		return lireval.Env{}, false, err
	}
	out := newFrame(o.outer)
	for _, fld := range o.fields {
		d, err := lireval.EvalDatum(fld.Expr, in)
		if err != nil {
			return lireval.Env{}, false, err
		}
		out[fld.Slot] = d
	}
	return out, true, nil
}

func (o *projectOp) Close() error { return o.in.Close() }
