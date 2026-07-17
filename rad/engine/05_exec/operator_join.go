package exec

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

type nljOp struct {
	l, r operator
	kind lir.JoinKind
	on   bound.Expr
	rOut lir.RowType

	right   []frame
	cur     *frame
	rPos    int
	matched bool
	primed  bool
}

func (o *nljOp) Next(ctx context.Context) (frame, bool, error) {
	if !o.primed {
		right, err := drainOp(ctx, o.r)
		if err != nil {
			return frame{}, false, err
		}
		o.right = right
		o.primed = true
	}
	for {
		if o.cur == nil {
			current, ok, err := o.l.Next(ctx)
			if err != nil || !ok {
				return frame{}, false, err
			}
			o.cur, o.rPos, o.matched = &current, 0, false
		}
		for o.rPos < len(o.right) {
			right := o.right[o.rPos]
			o.rPos++
			merged := mergeFrames(*o.cur, right)
			truth, err := bound.EvalPred(o.on, merged)
			if err != nil {
				return frame{}, false, err
			}
			if truth == lir.TriTrue {
				o.matched = true
				return merged, true, nil
			}
		}
		left := *o.cur
		o.cur = nil
		if o.kind == lir.LeftJoin && !o.matched {
			return o.nullPadded(left), true, nil
		}
	}
}

func (o *nljOp) nullPadded(left frame) frame {
	out := mergeFrames(left, frame{})
	for _, field := range o.rOut.Fields {
		out[field.Slot] = lir.NullDatum()
	}
	return out
}

func (o *nljOp) Close() error {
	err := o.l.Close()
	if rightErr := o.r.Close(); err == nil {
		err = rightErr
	}
	return err
}
