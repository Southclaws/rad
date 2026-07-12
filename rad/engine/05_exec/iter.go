package exec

// The operator seam for the relation-graph executor: a pull pipeline of
// Frames. Nothing in this contract requires materialisation — scans stream,
// filters stream, projections stream, slices stop pulling early — though
// blocking operators (sort, aggregate, attach) buffer internally. Full
// streaming end-to-end is a later arc; this seam is what makes it a change
// instead of a rewrite.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// Frame is one execution row: a datum per slot in scope. Scalar slots hold
// scalar datums, row- and array-typed slots hold materialised nested datums
// — one value vocabulary end to end, so any output can be referenced,
// reprojected, or spread regardless of its shape.
type Frame = bound.Env

// newFrame starts a frame from the outer environment — correlated
// sub-relations see their enclosing rows' slots exactly like their own.
func newFrame(outer bound.Env) Frame {
	f := make(Frame, len(outer)+8)
	for k, v := range outer {
		f[k] = v
	}
	return f
}

// mergeFrames unions two frames — slots are disjoint by construction.
func mergeFrames(l, r Frame) Frame {
	out := make(Frame, len(l)+len(r))
	for k, v := range l {
		out[k] = v
	}
	for k, v := range r {
		out[k] = v
	}
	return out
}

// Operator is the pull seam: Next yields the next frame until ok is false.
// Operators must be Closed; closing is idempotent.
type Operator interface {
	Next(ctx context.Context) (Frame, bool, error)
	Close() error
}

// frameToObject renders a frame as an object datum in the row type's field
// order. A missing slot is NULL — the one representation of absence.
func frameToObject(out lir.RowType, f Frame) lir.Datum {
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

// drainOp pulls every remaining frame. The caller owns Close.
func drainOp(ctx context.Context, op Operator) ([]Frame, error) {
	var out []Frame
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
