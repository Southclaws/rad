package query

// concatenateOp streams its inputs one after another, remapping each input's
// output slots onto the concatenation's fresh output slots positionally. The
// traversal order is an implementation detail — a concatenation's output has
// no logical order, so nothing downstream may observe it without an explicit
// sort.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

type concatenateOp struct {
	ins    []operator
	inOuts []lir.RowType
	out    lir.RowType
	outer  lireval.Env
	pos    int
}

func (o *concatenateOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	for o.pos < len(o.ins) {
		src, ok, err := o.ins[o.pos].Next(ctx)
		if err != nil {
			return lireval.Env{}, false, err
		}
		if !ok {
			o.pos++
			continue
		}
		return remapPositional(o.out, o.inOuts[o.pos], src, o.outer), true, nil
	}
	return lireval.Env{}, false, nil
}

func (o *concatenateOp) Close() error {
	var err error
	for _, in := range o.ins {
		if closeErr := in.Close(); err == nil {
			err = closeErr
		}
	}
	return err
}

// remapPositional rebuilds a source row on the output slots, field k of the
// source feeding field k of the output. A missing source slot stays missing,
// which downstream reads as NULL — the one representation of absence.
func remapPositional(out, srcOut lir.RowType, src lireval.Env, outer lireval.Env) lireval.Env {
	env := newFrame(outer)
	for k, field := range out.Fields {
		if d, has := src[srcOut.Fields[k].Slot]; has {
			env[field.Slot] = d
		}
	}
	return env
}
