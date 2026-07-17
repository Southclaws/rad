package exec

// distinctOp shares canonical row identity with recursive accumulation. It
// makes no ordering promise; ordered results require an explicit sort.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

type distinctOp struct {
	in    operator
	out   lir.RowType
	rows  []frame
	pos   int
	drawn bool
}

func (o *distinctOp) Next(ctx context.Context) (frame, bool, error) {
	if !o.drawn {
		frames, err := drainOp(ctx, o.in)
		if err != nil {
			return frame{}, false, err
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
		return frame{}, false, nil
	}
	f := o.rows[o.pos]
	o.pos++
	return f, true, nil
}

func (o *distinctOp) Close() error { return o.in.Close() }
