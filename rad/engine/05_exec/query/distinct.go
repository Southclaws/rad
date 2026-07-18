package query

// distinctOp shares canonical row identity with recursive accumulation. It
// makes no ordering promise; ordered results require an explicit sort.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

type distinctOp struct {
	in    operator
	out   lir.RowType
	rows  []lireval.Env
	pos   int
	drawn bool
}

func (o *distinctOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	if !o.drawn {
		frames, err := drainOp(ctx, o.in)
		if err != nil {
			return lireval.Env{}, false, err
		}
		seen := lireval.NewCanonicalRowSet(o.out.Fields)
		for _, f := range frames {
			if seen.Add(f) {
				o.rows = append(o.rows, f)
			}
		}
		o.drawn = true
	}
	if o.pos >= len(o.rows) {
		return lireval.Env{}, false, nil
	}
	f := o.rows[o.pos]
	o.pos++
	return f, true, nil
}

func (o *distinctOp) Close() error { return o.in.Close() }
