package exec

// The operator seam for the relation-graph executor: a pull pipeline of
// Frames. Nothing in this contract requires materialisation — scans stream,
// filters stream, slices stop pulling early — though blocking operators
// (sort, aggregate, batched projection) buffer internally. Full streaming
// end-to-end is a later arc; this seam is what makes it a change instead of
// a rewrite.

import (
	"context"

	lir "rad/rad/03_lir"
	"rad/rad/03_lir/bound"
)

// Frame is one execution row: scalar slot values (the expression
// environment) plus materialised nested values for row- and array-typed
// slots, which expressions never read — only projection builds them and
// only rendering consumes them.
type Frame struct {
	vals   bound.Env
	nested map[lir.SlotID]lir.Datum
}

// newFrame starts a frame from the outer environment — correlated
// sub-relations see their enclosing rows' slots exactly like their own.
func newFrame(outer bound.Env) Frame {
	vals := make(bound.Env, len(outer)+8)
	for k, v := range outer {
		vals[k] = v
	}
	return Frame{vals: vals}
}

func (f Frame) setNested(slot lir.SlotID, d lir.Datum) Frame {
	if f.nested == nil {
		f.nested = map[lir.SlotID]lir.Datum{}
	}
	f.nested[slot] = d
	return f
}

// Operator is the pull seam: Next yields the next frame until ok is false.
// Operators must be Closed; closing is idempotent.
type Operator interface {
	Next(ctx context.Context) (Frame, bool, error)
	Close() error
}

// frameToObject renders a frame as an object datum in the row type's field
// order: scalar fields from values, nested fields from materialised datums.
func frameToObject(out lir.RowType, f Frame) lir.Datum {
	fields := make([]lir.ObjectField, len(out.Fields))
	for i, fld := range out.Fields {
		var d lir.Datum
		if fld.Type.Kind.Scalar() {
			d = lir.ScalarDatum(f.vals[fld.Slot])
		} else if n, ok := f.nested[fld.Slot]; ok {
			d = n
		} else {
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
