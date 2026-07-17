package exec

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

type filterOp struct {
	in   operator
	pred bound.Expr
}

func (o *filterOp) Next(ctx context.Context) (frame, bool, error) {
	for {
		current, ok, err := o.in.Next(ctx)
		if err != nil || !ok {
			return frame{}, false, err
		}
		truth, err := bound.EvalPred(o.pred, current)
		if err != nil {
			return frame{}, false, err
		}
		if truth == lir.TriTrue {
			return current, true, nil
		}
	}
}

func (o *filterOp) Close() error { return o.in.Close() }
