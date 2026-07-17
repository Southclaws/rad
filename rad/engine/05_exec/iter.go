package exec

// Operators form a pull pipeline. Scans, filters, projections, and slices
// stream; sort, aggregate, distinct, and attach buffer their input.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

type frame = bound.Env

func newFrame(outer bound.Env) frame {
	f := make(frame, len(outer)+8)
	for k, v := range outer {
		f[k] = v
	}
	return f
}

func mergeFrames(l, r frame) frame {
	out := make(frame, len(l)+len(r))
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
	Next(ctx context.Context) (frame, bool, error)
	Close() error
}

// frameToObject renders a frame as an object datum in the row type's field
// order. A missing slot is NULL — the one representation of absence.
func frameToObject(out lir.RowType, f frame) lir.Datum {
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

func frameScalar(out lir.RowType, f frame) lir.Datum {
	if datum, ok := f[out.Fields[0].Slot]; ok {
		return datum
	}
	return lir.NullDatum()
}

func framesToArray(out lir.RowType, frames []frame) lir.Datum {
	elements := make([]lir.Datum, len(frames))
	for i, frame := range frames {
		elements[i] = frameToObject(out, frame)
	}
	return lir.ArrayDatum(elements)
}

func drainOp(ctx context.Context, op operator) ([]frame, error) {
	var out []frame
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

func drainAndClose(ctx context.Context, op operator) ([]frame, error) {
	frames, err := drainOp(ctx, op)
	if closeErr := op.Close(); err == nil {
		err = closeErr
	}
	return frames, err
}
