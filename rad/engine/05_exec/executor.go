package exec

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

type executor struct {
	view        kv.KV
	forceNested bool
	bindings    map[string][]frame
	frontier    map[string][]frame
	plans       map[string]planner.BindingPlan
	recur       RecursionLimits
}

func newExecutor(view kv.KV, recur RecursionLimits) *executor {
	return &executor{
		view:     view,
		bindings: map[string][]frame{},
		frontier: map[string][]frame{},
		plans:    map[string]planner.BindingPlan{},
		recur:    recur,
	}
}

// Materialised and recursive bindings are committed in dependency order.
// Replayed bindings are evaluated lazily at their sole occurrence.
func (ex *executor) commit(ctx context.Context, bindings []planner.BindingPlan) error {
	for _, binding := range bindings {
		ex.plans[binding.Name] = binding
	}
	for _, binding := range bindings {
		if binding.Recursive {
			frames, err := ex.commitRecursive(ctx, binding)
			if err != nil {
				return err
			}
			ex.bindings[binding.Name] = frames
			continue
		}
		if binding.Strategy != planner.BindingMaterialise {
			continue
		}
		op, err := ex.build(ctx, binding.Plan, bound.Env{})
		if err != nil {
			return err
		}
		frames, err := drainAndClose(ctx, op)
		if err != nil {
			return err
		}
		ex.bindings[binding.Name] = frames
	}
	return nil
}

func (ex *executor) build(ctx context.Context, node planner.PhysNode, outer bound.Env) (operator, error) {
	switch n := node.(type) {
	case *planner.PKGetExec:
		return ex.buildPKGet(ctx, n, outer)
	case *planner.TableScanExec:
		it, err := scanTable(ctx, ex.view, n.Scan.Table)
		if err != nil {
			return nil, err
		}
		return &rowIterOp{scan: n.Scan, it: it, outer: outer}, nil
	case *planner.RowsExec:
		return &rowsOp{n: n.Rows, outer: outer}, nil
	case *planner.IndexRangeScanExec:
		return ex.buildIndexScan(ctx, n, outer)
	case *planner.FilterExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &filterOp{in: in, pred: n.Pred}, nil
	case *planner.AttachExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &attachOp{ex: ex, in: in, specs: n.Specs, outer: outer}, nil
	case *planner.ProjectExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &projectOp{in: in, fields: n.Fields, outer: outer}, nil
	case *planner.SortExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &sortOp{in: in, terms: n.Terms}, nil
	case *planner.SliceExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &sliceOp{in: in, offset: n.Offset, limit: n.Limit}, nil
	case *planner.AggregateExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &aggOp{in: in, groups: n.Groups, terms: n.Terms, outer: outer}, nil
	case *planner.NestedLoopJoinExec:
		left, err := ex.build(ctx, n.L, outer)
		if err != nil {
			return nil, err
		}
		right, err := ex.build(ctx, n.R, outer)
		if err != nil {
			left.Close()
			return nil, err
		}
		return &nljOp{l: left, r: right, kind: n.Kind, on: n.On, rOut: n.ROut}, nil
	case *planner.RefExec:
		if frames, ok := ex.bindings[n.Binding]; ok {
			return &refOp{n: n, frames: frames, outer: outer}, nil
		}
		binding, ok := ex.plans[n.Binding]
		if !ok {
			return nil, fmt.Errorf("exec: binding %q was not committed before its occurrence", n.Binding)
		}
		in, err := ex.build(ctx, binding.Plan, bound.Env{})
		if err != nil {
			return nil, err
		}
		return &replayRefOp{n: n, in: in, outer: outer}, nil
	case *planner.RecursiveRefExec:
		return &recursiveRefOp{n: n, frames: ex.frontier[n.Binding], outer: outer}, nil
	case *planner.DistinctExec:
		in, err := ex.build(ctx, n.Input, outer)
		if err != nil {
			return nil, err
		}
		return &distinctOp{in: in, out: n.Out}, nil
	}
	return nil, fmt.Errorf("exec: unknown physical node %T", node)
}
