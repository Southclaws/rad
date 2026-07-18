package query

import (
	"context"

	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

type sliceOp struct {
	in      operator
	offset  int
	limit   *int
	skipped int
	emitted int
}

func (o *sliceOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	if o.limit != nil && o.emitted >= *o.limit {
		return lireval.Env{}, false, nil
	}
	for o.skipped < o.offset {
		_, ok, err := o.in.Next(ctx)
		if err != nil || !ok {
			return lireval.Env{}, false, err
		}
		o.skipped++
	}
	current, ok, err := o.in.Next(ctx)
	if err != nil || !ok {
		return lireval.Env{}, false, err
	}
	o.emitted++
	return current, true, nil
}

func (o *sliceOp) Close() error { return o.in.Close() }
