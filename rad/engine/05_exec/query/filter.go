package query

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

type filterOp struct {
	in   operator
	pred bound.Expr
}

func (o *filterOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	for {
		current, ok, err := o.in.Next(ctx)
		if err != nil || !ok {
			return lireval.Env{}, false, err
		}
		truth, err := lireval.EvalPred(o.pred, current)
		if err != nil {
			return lireval.Env{}, false, err
		}
		if truth == lir.TriTrue {
			return current, true, nil
		}
	}
}

func (o *filterOp) Close() error { return o.in.Close() }
