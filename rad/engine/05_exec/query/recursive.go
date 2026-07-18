package query

// Recursive bindings use semi-naive evaluation: the anchor seeds a frontier,
// and each round evaluates the step against only that frontier. Admit-new
// accumulation compares candidates against the complete result.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	planner "github.com/Southclaws/rad/rad/engine/04_planner/physical"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Canonical slots give outer references and recursive references the same row
// identity.
func (ex *Executor) commitRecursive(ctx context.Context, b planner.BindingPlan) ([]lireval.Env, error) {
	canon := b.Out.Fields
	anchorSrc := map[string]lir.SlotID{}
	for _, f := range canon {
		anchorSrc[f.Name] = f.Slot
	}
	stepSrc := map[string]lir.SlotID{}
	for _, f := range b.StepOut.Fields {
		stepSrc[f.Name] = f.Slot
	}
	project := func(src lireval.Env, srcSlot map[string]lir.SlotID) lireval.Env {
		out := make(lireval.Env, len(canon))
		for _, cf := range canon {
			if d, ok := src[srcSlot[cf.Name]]; ok {
				out[cf.Slot] = d
			}
		}
		return out
	}

	var seen *lireval.CanonicalRowSet
	if b.Accumulation == lir.AccumulateNew {
		seen = lireval.NewCanonicalRowSet(canon)
	}
	admit := func(f lireval.Env) (lireval.Env, bool) {
		if seen == nil {
			return f, true
		}
		if !seen.Add(f) {
			return nil, false
		}
		return f, true
	}

	anchorOp, err := ex.build(ctx, b.Plan, lireval.Env{})
	if err != nil {
		return nil, err
	}
	anchorFrames, err := drainAndClose(ctx, anchorOp)
	if err != nil {
		return nil, err
	}

	var result, frontier []lireval.Env
	for _, f := range anchorFrames {
		if c, ok := admit(project(f, anchorSrc)); ok {
			result = append(result, c)
			frontier = append(frontier, c)
		}
	}

	for i := 0; len(frontier) > 0; i++ {
		if i >= ex.recur.MaxIterations {
			return nil, reject.Fail(reject.ReasonRecursionLimit, "exec: recursive binding %q did not terminate within %d iterations", b.Name, ex.recur.MaxIterations)
		}
		ex.frontier[b.Name] = frontier
		stepOp, err := ex.build(ctx, b.Step, lireval.Env{})
		if err != nil {
			return nil, err
		}
		produced, err := drainAndClose(ctx, stepOp)
		if err != nil {
			return nil, err
		}
		var next []lireval.Env
		for _, f := range produced {
			if c, ok := admit(project(f, stepSrc)); ok {
				next = append(next, c)
			}
		}
		result = append(result, next...)
		if len(result) > ex.recur.MaxRows {
			return nil, reject.Fail(reject.ReasonRecursionLimit, "exec: recursive binding %q produced more than %d rows", b.Name, ex.recur.MaxRows)
		}
		frontier = next
	}
	delete(ex.frontier, b.Name)
	return result, nil
}

// recursiveRefOp streams one occurrence of a recursive binding's frontier.
type recursiveRefOp struct {
	n      *planner.RecursiveRefExec
	frames []lireval.Env
	outer  lireval.Env
	pos    int
}

func (o *recursiveRefOp) Next(context.Context) (lireval.Env, bool, error) {
	if o.pos >= len(o.frames) {
		return lireval.Env{}, false, nil
	}
	src := o.frames[o.pos]
	o.pos++
	return remapCanon(o.n.Out, o.n.Canon, src, o.outer), true, nil
}

func (o *recursiveRefOp) Close() error { return nil }
