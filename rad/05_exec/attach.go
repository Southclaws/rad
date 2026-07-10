package exec

// projectOp: shape assembly, and the home of deduplicated correlated
// execution. Scalar fields evaluate per frame; attached crossings
// materialise as explicit sub-plans —
//
//   key-correlated:  dedupe the outer key tuples across the batch, run the
//                    inner plan once per DISTINCT key (a NULL key component
//                    short-circuits to the empty result with no KV work),
//                    group and attach; grandchildren batch across each
//                    inner batch in turn.
//   uncorrelated:    run once, share the result with every frame.
//   general:         per-frame nested evaluation — the fallback.
//
// All three strategies are result-equivalent; the conformance suite asserts
// batched ≡ nested.

import (
	"context"
	"fmt"

	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
	planner "rad/rad/04_planner"
)

type projectOp struct {
	ex     *executor
	in     Operator
	fields []planner.PhysField
	outer  bound.Env

	out    []Frame
	pos    int
	primed bool
}

func (o *projectOp) Next(ctx context.Context) (Frame, bool, error) {
	if !o.primed {
		if err := o.project(ctx); err != nil {
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

func (o *projectOp) Close() error { return o.in.Close() }

func (o *projectOp) project(ctx context.Context) error {
	batch, err := drainOp(ctx, o.in)
	if err != nil {
		return err
	}
	if len(batch) == 0 {
		return nil
	}

	// Output frames start from the outer environment so correlated
	// references above this projection keep resolving.
	outFrames := make([]Frame, len(batch))
	for i := range batch {
		outFrames[i] = newFrame(o.outer)
	}

	glue := o.ex.glue(o.outer)
	for _, fld := range o.fields {
		if fld.Attach == nil {
			for i, in := range batch {
				v, err := bound.Eval(ctx, fld.Expr, in.vals, glue)
				if err != nil {
					return err
				}
				outFrames[i].vals[fld.Slot] = v
			}
			continue
		}
		if err := o.attach(ctx, fld, batch, outFrames); err != nil {
			return err
		}
	}
	o.out = outFrames
	return nil
}

func (o *projectOp) attach(ctx context.Context, fld planner.PhysField, batch, outFrames []Frame) error {
	a := fld.Attach

	switch {
	case a.Corr.Kind == planner.Uncorrelated:
		d, sv, err := o.runAttach(ctx, a, o.outer)
		if err != nil {
			return err
		}
		for i := range outFrames {
			assignAttach(&outFrames[i], fld, a, d, sv)
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
		empties := make([]int, 0)

		for i, in := range batch {
			keyVals := make([]qir.Value, len(a.Corr.Keys))
			null := false
			for j, k := range a.Corr.Keys {
				v := in.vals[k.OuterSlot]
				if v.Null {
					null = true
					break
				}
				keyVals[j] = v
			}
			if null {
				// Equality with NULL matches nothing: the empty result,
				// with no KV work at all.
				empties = append(empties, i)
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
					env[k.OuterSlot] = keyVals[j]
				}
				g = &keyGroup{env: env}
				groups[string(enc)] = g
				order = append(order, string(enc))
			}
			g.frames = append(g.frames, i)
		}

		for _, key := range order {
			g := groups[key]
			d, sv, err := o.runAttach(ctx, a, g.env)
			if err != nil {
				return err
			}
			for _, i := range g.frames {
				assignAttach(&outFrames[i], fld, a, d, sv)
			}
		}
		for _, i := range empties {
			d, sv := emptyAttach(a)
			assignAttach(&outFrames[i], fld, a, d, sv)
		}
		return nil

	default: // general correlation, or batching disabled
		for i, in := range batch {
			d, sv, err := o.runAttach(ctx, a, in.vals)
			if err != nil {
				return err
			}
			assignAttach(&outFrames[i], fld, a, d, sv)
		}
		return nil
	}
}

// runAttach executes the attached plan under env and folds the resulting
// frames into the crossing's shape: a nested datum for first/array, a
// scalar value for scalar/exists.
func (o *projectOp) runAttach(ctx context.Context, a *planner.AttachSpec, env bound.Env) (qir.Datum, qir.Value, error) {
	op, err := o.ex.build(ctx, a.Plan, env)
	if err != nil {
		return qir.Datum{}, qir.Value{}, err
	}
	defer op.Close()

	switch a.Kind {
	case planner.CrossExists:
		_, ok, err := op.Next(ctx)
		if err != nil {
			return qir.Datum{}, qir.Value{}, err
		}
		return qir.Datum{}, qir.Bool(ok), nil

	case planner.CrossFirst:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return qir.Datum{}, qir.Value{}, err
		}
		if !ok {
			return qir.NullDatum(), qir.Value{}, nil
		}
		return frameToObject(a.Out, f), qir.Value{}, nil

	case planner.CrossScalar:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return qir.Datum{}, qir.Value{}, err
		}
		fld := a.Out.Fields[0]
		if !ok {
			return qir.Datum{}, qir.Null(fld.Type.Kind.CatalogType()), nil
		}
		return qir.Datum{}, f.vals[fld.Slot], nil

	case planner.CrossArray:
		frames, err := drainOp(ctx, op)
		if err != nil {
			return qir.Datum{}, qir.Value{}, err
		}
		elems := make([]qir.Datum, len(frames))
		for i, f := range frames {
			elems[i] = frameToObject(a.Out, f)
		}
		return qir.ArrayDatum(elems), qir.Value{}, nil
	}
	return qir.Datum{}, qir.Value{}, fmt.Errorf("exec: unknown crossing %q", a.Kind)
}

// emptyAttach is the no-KV-work result for a NULL correlation key.
func emptyAttach(a *planner.AttachSpec) (qir.Datum, qir.Value) {
	switch a.Kind {
	case planner.CrossExists:
		return qir.Datum{}, qir.Bool(false)
	case planner.CrossScalar:
		return qir.Datum{}, qir.Null(a.Out.Fields[0].Type.Kind.CatalogType())
	case planner.CrossFirst:
		return qir.NullDatum(), qir.Value{}
	default: // array
		return qir.ArrayDatum(nil), qir.Value{}
	}
}

// assignAttach writes a crossing's result to the right side of the frame:
// nested datums for first/array, scalar values for scalar/exists.
func assignAttach(f *Frame, fld planner.PhysField, a *planner.AttachSpec, d qir.Datum, sv qir.Value) {
	switch a.Kind {
	case planner.CrossFirst, planner.CrossArray:
		*f = f.setNested(fld.Slot, d)
	default:
		f.vals[fld.Slot] = sv
	}
}
