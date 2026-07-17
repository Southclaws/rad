package exec

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

func remapOccurrence(n *planner.RefExec, src frame, outer bound.Env) frame {
	return remapCanon(n.Out, n.Canon, src, outer)
}

// Remapping preserves the outer environment and never mutates the canonical
// frame shared by other binding occurrences.
func remapCanon(out lir.RowType, canon []lir.SlotID, src frame, outer bound.Env) frame {
	remapped := newFrame(outer)
	for i, field := range out.Fields {
		if datum, ok := src[canon[i]]; ok {
			remapped[field.Slot] = datum
		}
	}
	return remapped
}

type refOp struct {
	n      *planner.RefExec
	frames []frame
	outer  bound.Env
	pos    int
}

func (o *refOp) Next(context.Context) (frame, bool, error) {
	if o.pos >= len(o.frames) {
		return frame{}, false, nil
	}
	src := o.frames[o.pos]
	o.pos++
	return remapOccurrence(o.n, src, o.outer), true, nil
}

func (o *refOp) Close() error { return nil }

type replayRefOp struct {
	n     *planner.RefExec
	in    operator
	outer bound.Env
}

func (o *replayRefOp) Next(ctx context.Context) (frame, bool, error) {
	src, ok, err := o.in.Next(ctx)
	if err != nil || !ok {
		return frame{}, false, err
	}
	return remapOccurrence(o.n, src, o.outer), true, nil
}

func (o *replayRefOp) Close() error { return o.in.Close() }

func resolveConst(value planner.ConstVal, outer bound.Env) (lir.Value, error) {
	if value.Lit != nil {
		return *value.Lit, nil
	}
	return outer.ScalarAt(*value.Outer, "outer", lir.Type{})
}

type rowsOp struct {
	n     *bound.Rows
	outer bound.Env
	pos   int
}

func (o *rowsOp) Next(context.Context) (frame, bool, error) {
	if o.pos >= len(o.n.Vals) {
		return frame{}, false, nil
	}
	cells := o.n.Vals[o.pos]
	o.pos++
	frame := newFrame(o.outer)
	for i, field := range o.n.Output().Fields {
		frame.SetScalar(field.Slot, cells[i])
	}
	return frame, true, nil
}

func (o *rowsOp) Close() error { return nil }

func rowToFrame(scan *bound.Scan, row lir.Row, outer bound.Env) frame {
	frame := newFrame(outer)
	for _, field := range scan.Output().Fields {
		frame.SetScalar(field.Slot, row[field.Name])
	}
	return frame
}
