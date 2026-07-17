package exec

// The recursive-binding fixpoint: semi-naive evaluation over the pull
// operators. The anchor seeds the result and the working table (frontier);
// each round the step runs with the frontier published for its
// recursive_ref, and the rows it produces — admitted against the whole result
// under admit-new accumulation, by the shared canonical row identity — become
// the next frontier and extend the result, until the frontier is empty.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// commitRecursive computes a recursive binding's fixpoint. Rows are projected
// onto the binding's canonical slots so the outer ref and the step's
// recursive_ref read them identically, exactly as a materialised binding's
// canonical frames are read.
func (ex *executor) commitRecursive(ctx context.Context, b planner.BindingPlan) ([]Frame, error) {
	canon := b.Out.Fields
	anchorSrc := map[string]lir.SlotID{}
	for _, f := range canon {
		anchorSrc[f.Name] = f.Slot
	}
	stepSrc := map[string]lir.SlotID{}
	for _, f := range b.StepOut.Fields {
		stepSrc[f.Name] = f.Slot
	}
	project := func(src Frame, srcSlot map[string]lir.SlotID) Frame {
		out := make(Frame, len(canon))
		for _, cf := range canon {
			if d, ok := src[srcSlot[cf.Name]]; ok {
				out[cf.Slot] = d
			}
		}
		return out
	}

	var seen *bound.CanonicalRowSet
	if b.Accumulation == lir.AccumulateNew {
		seen = bound.NewCanonicalRowSet(canon)
	}
	admit := func(f Frame) (Frame, bool) {
		if seen == nil {
			return f, true
		}
		if !seen.Add(f) {
			return nil, false
		}
		return f, true
	}

	anchorOp, err := ex.build(ctx, b.Plan, bound.Env{})
	if err != nil {
		return nil, err
	}
	anchorFrames, err := drainOp(ctx, anchorOp)
	anchorOp.Close()
	if err != nil {
		return nil, err
	}

	var result, frontier []Frame
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
		stepOp, err := ex.build(ctx, b.Step, bound.Env{})
		if err != nil {
			return nil, err
		}
		produced, err := drainOp(ctx, stepOp)
		stepOp.Close()
		if err != nil {
			return nil, err
		}
		var next []Frame
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
	frames []Frame
	outer  bound.Env
	pos    int
}

func (o *recursiveRefOp) Next(context.Context) (Frame, bool, error) {
	if o.pos >= len(o.frames) {
		return Frame{}, false, nil
	}
	src := o.frames[o.pos]
	o.pos++
	return remapCanon(o.n.Out, o.n.Canon, src, o.outer), true, nil
}

func (o *recursiveRefOp) Close() error { return nil }

