package exec

// attachOp: the home of deduplicated correlated execution. Every crossing
// the planner extracted — from a projection field, a filter predicate, an
// order term, an aggregate argument — materialises here as an explicit
// sub-plan writing one slot, by strategy:
//
//   key-correlated:  dedupe the outer key tuples across the batch, run the
//                    inner plan once per DISTINCT key (a NULL key component
//                    short-circuits to the empty result with no KV work),
//                    and share the result among the frames of each key;
//                    grandchildren batch across each inner batch in turn.
//   uncorrelated:    run once, share the result with every frame.
//   general:         per-frame nested evaluation — the fallback.
//
// All three strategies are result-equivalent; the conformance suite asserts
// batched ≡ nested. Frames re-emit in input order, so attaching preserves
// any provided ordering.

import (
	"context"
	"fmt"

	lir "rad/rad/03_lir"
	"rad/rad/03_lir/bound"
	planner "rad/rad/04_planner"
)

type attachOp struct {
	ex    *executor
	in    Operator
	specs []*planner.AttachSpec
	outer bound.Env

	out    []Frame
	pos    int
	primed bool
}

func (o *attachOp) Next(ctx context.Context) (Frame, bool, error) {
	if !o.primed {
		if err := o.attachAll(ctx); err != nil {
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

func (o *attachOp) attach(ctx context.Context, a *planner.AttachSpec, batch []Frame) error {
	switch {
	case a.Corr.Kind == planner.Uncorrelated:
		d, err := o.runAttach(ctx, a, o.outer)
		if err != nil {
			return err
		}
		for i := range batch {
			batch[i][a.Slot] = d
		}
		return nil

	case a.Corr.Kind == planner.KeyCorrelated && !o.ex.forceNested:
		// Dedupe the outer key tuples over the batch; fetch per DISTINCT key.
		type keyGroup struct {
			env    bound.Env
			frames []int
		}
		groups := map[string]*keyGroup{}
		var order []string

		for i, in := range batch {
			keyVals := make([]lir.Value, len(a.Corr.Keys))
			null := false
			for j, k := range a.Corr.Keys {
				v, err := in.ScalarAt(k.OuterSlot, k.InnerCol, lir.Type{})
				if err != nil {
					return err
				}
				if v.Null {
					null = true
					break
				}
				keyVals[j] = v
			}
			if null {
				// Equality with NULL matches nothing: the empty result,
				// with no KV work at all.
				batch[i][a.Slot] = emptyAttach(a)
				continue
			}
			enc, err := EncodeTuple(keyVals)
			if err != nil {
				return err
			}
			g, ok := groups[string(enc)]
			if !ok {
				env := make(bound.Env, len(o.outer)+len(a.Corr.Keys))
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

// runAttach executes the attached plan under env and folds the resulting
// frames into the crossing's shape — every shape is a datum.
func (o *attachOp) runAttach(ctx context.Context, a *planner.AttachSpec, env bound.Env) (lir.Datum, error) {
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
		d, has := f[a.Out.Fields[0].Slot]
		if !has {
			return lir.NullDatum(), nil
		}
		return d, nil

	case planner.CrossArray:
		frames, err := drainOp(ctx, op)
		if err != nil {
			return lir.Datum{}, err
		}
		elems := make([]lir.Datum, len(frames))
		for i, f := range frames {
			elems[i] = frameToObject(a.Out, f)
		}
		return lir.ArrayDatum(elems), nil
	}
	return lir.Datum{}, fmt.Errorf("exec: unknown crossing %q", a.Kind)
}

// emptyAttach is the no-KV-work result for a NULL correlation key.
func emptyAttach(a *planner.AttachSpec) lir.Datum {
	switch a.Kind {
	case planner.CrossExists:
		return lir.ScalarDatum(lir.Bool(false))
	case planner.CrossArray:
		return lir.ArrayDatum(nil)
	default: // scalar, first
		return lir.NullDatum()
	}
}

// projectOp assembles output rows from pure expressions, streaming — the
// blocking batch point moved into attachOp, which exists only when a field
// actually needs one.
type projectOp struct {
	in     Operator
	fields []planner.PhysField
	outer  bound.Env
}

func (o *projectOp) Next(ctx context.Context) (Frame, bool, error) {
	in, ok, err := o.in.Next(ctx)
	if err != nil || !ok {
		return Frame{}, false, err
	}
	out := newFrame(o.outer)
	for _, fld := range o.fields {
		d, err := bound.EvalDatum(fld.Expr, in)
		if err != nil {
			return Frame{}, false, err
		}
		out[fld.Slot] = d
	}
	return out, true, nil
}

func (o *projectOp) Close() error { return o.in.Close() }
