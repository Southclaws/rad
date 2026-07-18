package query

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

type nljOp struct {
	l, r operator
	kind lir.JoinKind
	on   bound.Expr
	rOut lir.RowType

	right   []lireval.Env
	cur     *lireval.Env
	rPos    int
	matched bool
	primed  bool
}

func (o *nljOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	if !o.primed {
		right, err := drainOp(ctx, o.r)
		if err != nil {
			return lireval.Env{}, false, err
		}
		o.right = right
		o.primed = true
	}
	for {
		if o.cur == nil {
			current, ok, err := o.l.Next(ctx)
			if err != nil || !ok {
				return lireval.Env{}, false, err
			}
			o.cur, o.rPos, o.matched = &current, 0, false
		}
		for o.rPos < len(o.right) {
			right := o.right[o.rPos]
			o.rPos++
			merged := mergeFrames(*o.cur, right)
			truth, err := lireval.EvalPred(o.on, merged)
			if err != nil {
				return lireval.Env{}, false, err
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

func (o *nljOp) nullPadded(left lireval.Env) lireval.Env {
	out := mergeFrames(left, lireval.Env{})
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
