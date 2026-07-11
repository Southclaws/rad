package exec

// The RelEvaluator glue: crossings that appear inside scalar expressions —
// an Exists in a filter predicate, a Scalar in a computed field — evaluate
// by planning and running their sub-relation as an ordinary operator tree
// under the current row's environment. Plans are cached per relation node;
// this is the nested path, also used when batching is disabled.

import (
	"context"

	lir "rad/rad/03_lir"
	"rad/rad/03_lir/bound"
	planner "rad/rad/04_planner"
)

type relGlue struct {
	ex    *executor
	outer bound.Env
}

func (ex *executor) glue(outer bound.Env) bound.RelEvaluator {
	return relGlue{ex: ex, outer: outer}
}

func (g relGlue) planFor(rel bound.Relation) planner.PhysNode {
	if p, ok := g.ex.plans[rel]; ok {
		return p
	}
	p := planner.PlanRelation(rel)
	g.ex.plans[rel] = p
	return p
}

func (g relGlue) Exists(ctx context.Context, rel bound.Relation, env bound.Env) (bool, error) {
	op, err := g.ex.build(ctx, g.planFor(rel), env)
	if err != nil {
		return false, err
	}
	defer op.Close()
	_, ok, err := op.Next(ctx)
	return ok, err
}

func (g relGlue) Scalar(ctx context.Context, rel bound.Relation, env bound.Env) (lir.Value, error) {
	op, err := g.ex.build(ctx, g.planFor(rel), env)
	if err != nil {
		return lir.Value{}, err
	}
	defer op.Close()
	f, ok, err := op.Next(ctx)
	if err != nil {
		return lir.Value{}, err
	}
	fld := rel.Output().Fields[0]
	if !ok {
		return lir.Null(fld.Type.Kind.CatalogType()), nil
	}
	return f.vals[fld.Slot], nil
}
