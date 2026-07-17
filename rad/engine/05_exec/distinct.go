package exec

// The distinct operator: blocking deduplication by canonical full-row identity
// (bound.CanonicalRowSet, shared with recursive admit-new accumulation, so the
// two can never disagree). It drains its input, admits each canonical row once,
// and emits the survivors. It makes no ordering promise — an ordered distinct
// result needs an explicit order above it — so first-occurrence emission is an
// implementation detail, not a contract.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

type distinctOp struct {
	in    Operator
	out   lir.RowType
	rows  []Frame
	pos   int
	drawn bool
}

func (o *distinctOp) Next(ctx context.Context) (Frame, bool, error) {
	if !o.drawn {
		frames, err := drainOp(ctx, o.in)
		if err != nil {
			return Frame{}, false, err
		}
		seen := bound.NewCanonicalRowSet(o.out.Fields)
		for _, f := range frames {
			if seen.Add(f) {
				o.rows = append(o.rows, f)
			}
		}
		o.drawn = true
	}
	if o.pos >= len(o.rows) {
		return Frame{}, false, nil
	}
	f := o.rows[o.pos]
	o.pos++
	return f, true, nil
}

func (o *distinctOp) Close() error { return o.in.Close() }
