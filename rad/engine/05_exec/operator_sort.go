package exec

import (
	"context"
	"fmt"
	"slices"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

type sortOp struct {
	in     operator
	terms  []bound.OrderTerm
	sorted []frame
	pos    int
	primed bool
}

func (o *sortOp) Next(ctx context.Context) (frame, bool, error) {
	if !o.primed {
		frames, err := drainOp(ctx, o.in)
		if err != nil {
			return frame{}, false, err
		}
		keys := make([][]lir.Value, len(frames))
		for i, current := range frames {
			keys[i] = make([]lir.Value, len(o.terms))
			for j, term := range o.terms {
				value, err := bound.Eval(term.Expr, current)
				if err != nil {
					return frame{}, false, err
				}
				keys[i][j] = value
			}
		}
		positions := make([]int, len(frames))
		for i := range positions {
			positions[i] = i
		}
		var sortErr error
		slices.SortStableFunc(positions, func(a, b int) int {
			for j, term := range o.terms {
				comparison, err := keys[a][j].Compare(keys[b][j])
				if err != nil && sortErr == nil {
					sortErr = err
				}
				if term.Desc {
					comparison = -comparison
				}
				if comparison != 0 {
					return comparison
				}
			}
			return 0
		})
		if sortErr != nil {
			return frame{}, false, fmt.Errorf("exec: %w", sortErr)
		}
		o.sorted = make([]frame, len(frames))
		for i, position := range positions {
			o.sorted[i] = frames[position]
		}
		o.primed = true
	}
	if o.pos >= len(o.sorted) {
		return frame{}, false, nil
	}
	current := o.sorted[o.pos]
	o.pos++
	return current, true, nil
}

func (o *sortOp) Close() error { return o.in.Close() }
