package query

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	"github.com/Southclaws/rad/rad/engine/04_planner/analysis"
	planner "github.com/Southclaws/rad/rad/engine/04_planner/physical"
)

func remapOccurrence(n *planner.RefExec, src lireval.Env, outer lireval.Env) lireval.Env {
	return remapCanon(n.Out, n.Canon, src, outer)
}

// Remapping preserves the outer environment and never mutates the canonical
// frame shared by other binding occurrences.
func remapCanon(out lir.RowType, canon []lir.SlotID, src lireval.Env, outer lireval.Env) lireval.Env {
	remapped := newFrame(outer)
	for i, field := range out.Fields {
		if datum, ok := src[canon[i]]; ok {
			remapped[field.Slot] = datum
		}
	}
	return remapped
}

type refOp struct {
	n      *planner.RefExec
	frames []lireval.Env
	outer  lireval.Env
	pos    int
}

func (o *refOp) Next(context.Context) (lireval.Env, bool, error) {
	if o.pos >= len(o.frames) {
		return lireval.Env{}, false, nil
	}
	src := o.frames[o.pos]
	o.pos++
	return remapOccurrence(o.n, src, o.outer), true, nil
}

func (o *refOp) Close() error { return nil }

type replayRefOp struct {
	n     *planner.RefExec
	in    operator
	outer lireval.Env
}

func (o *replayRefOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	src, ok, err := o.in.Next(ctx)
	if err != nil || !ok {
		return lireval.Env{}, false, err
	}
	return remapOccurrence(o.n, src, o.outer), true, nil
}

func (o *replayRefOp) Close() error { return o.in.Close() }

func resolveConst(value analysis.ConstVal, outer lireval.Env) (lir.Value, error) {
	if value.Lit != nil {
		return *value.Lit, nil
	}
	return outer.ScalarAt(*value.Outer, "outer", lir.Type{})
}

type rowsOp struct {
	n     *bound.Rows
	outer lireval.Env
	pos   int
}

func (o *rowsOp) Next(context.Context) (lireval.Env, bool, error) {
	if o.pos >= len(o.n.Vals) {
		return lireval.Env{}, false, nil
	}
	cells := o.n.Vals[o.pos]
	o.pos++
	env := newFrame(o.outer)
	for i, field := range o.n.Output().Fields {
		env.SetScalar(field.Slot, cells[i])
	}
	return env, true, nil
}

func (o *rowsOp) Close() error { return nil }

func rowToFrame(scan *bound.Scan, row lir.Row, outer lireval.Env) lireval.Env {
	env := newFrame(outer)
	for _, field := range scan.Output().Fields {
		env.SetScalar(field.Slot, row[field.Name])
	}
	return env
}
