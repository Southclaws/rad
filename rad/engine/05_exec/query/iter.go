package query

// Operators form a pull pipeline. Scans, filters, projections, and slices
// stream; sort, aggregate, distinct, and attach buffer their input.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

func newFrame(outer lireval.Env) lireval.Env {
	f := make(lireval.Env, len(outer)+8)
	for k, v := range outer {
		f[k] = v
	}
	return f
}

func mergeFrames(l, r lireval.Env) lireval.Env {
	out := make(lireval.Env, len(l)+len(r))
	for k, v := range l {
		out[k] = v
	}
	for k, v := range r {
		out[k] = v
	}
	return out
}

// Closing an operator is idempotent.
type operator interface {
	Next(ctx context.Context) (lireval.Env, bool, error)
	Close() error
}

// frameToObject renders an evaluation frame as an object datum in the row type's field
// order. A missing slot is NULL — the one representation of absence.
func frameToObject(out lir.RowType, f lireval.Env) lir.Datum {
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

func frameScalar(out lir.RowType, f lireval.Env) lir.Datum {
	if datum, ok := f[out.Fields[0].Slot]; ok {
		return datum
	}
	return lir.NullDatum()
}

func framesToArray(out lir.RowType, frames []lireval.Env) lir.Datum {
	elements := make([]lir.Datum, len(frames))
	for i, env := range frames {
		elements[i] = frameToObject(out, env)
	}
	return lir.ArrayDatum(elements)
}

func drainOp(ctx context.Context, op operator) ([]lireval.Env, error) {
	var out []lireval.Env
	for {
		f, ok, err := op.Next(ctx)
		if err != nil {
			return nil, err
		}
		if !ok {
			return out, nil
		}
		out = append(out, f)
	}
}

func drainAndClose(ctx context.Context, op operator) ([]lireval.Env, error) {
	frames, err := drainOp(ctx, op)
	if closeErr := op.Close(); err == nil {
		err = closeErr
	}
	return frames, err
}
