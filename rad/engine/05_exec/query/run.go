package query

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func (ex *Executor) SetForceNested(force bool) { ex.forceNested = force }

func (ex *Executor) SeedBindings(bindings map[string][]lireval.Env) {
	ex.bindings = bindings
}

func (ex *Executor) RunFrames(ctx context.Context, plan *physical.PhysPlan) ([]lireval.Env, error) {
	if err := store.AdmitCatalogDependencies(ctx, ex.view, plan.Dependencies); err != nil {
		return nil, err
	}
	if err := ex.commit(ctx, plan.Bindings); err != nil {
		return nil, err
	}
	op, err := ex.build(ctx, plan.Root, lireval.Env{})
	if err != nil {
		return nil, err
	}
	return drainAndClose(ctx, op)
}

func (ex *Executor) Execute(ctx context.Context, plan *physical.PhysPlan) (lir.Datum, error) {
	if err := store.AdmitCatalogDependencies(ctx, ex.view, plan.Dependencies); err != nil {
		return lir.Datum{}, err
	}
	if err := ex.commit(ctx, plan.Bindings); err != nil {
		return lir.Datum{}, err
	}
	op, err := ex.build(ctx, plan.Root, lireval.Env{})
	if err != nil {
		return lir.Datum{}, err
	}
	defer op.Close()

	switch plan.Card {
	case lir.CardFirst:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return lir.Datum{}, err
		}
		if !ok {
			return lir.NullDatum(), nil
		}
		return frameToObject(plan.Out, f), nil
	case lir.CardExactlyOne:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return lir.Datum{}, err
		}
		if !ok {
			return lir.Datum{}, reject.Runtimef("exec: expected exactly one row, got none")
		}
		if _, more, err := op.Next(ctx); err != nil {
			return lir.Datum{}, err
		} else if more {
			return lir.Datum{}, reject.Runtimef("exec: expected exactly one row, got more")
		}
		return frameToObject(plan.Out, f), nil
	case lir.CardScalar:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return lir.Datum{}, err
		}
		if !ok {
			return lir.NullDatum(), nil
		}
		return frameScalar(plan.Out, f), nil
	default:
		frames, err := drainOp(ctx, op)
		if err != nil {
			return lir.Datum{}, err
		}
		return framesToArray(plan.Out, frames), nil
	}
}

func ShapeFrames(card lir.RootCard, out lir.RowType, frames []lireval.Env) (lir.Datum, error) {
	switch card {
	case lir.CardFirst:
		if len(frames) == 0 {
			return lir.NullDatum(), nil
		}
		return frameToObject(out, frames[0]), nil
	case lir.CardExactlyOne:
		if len(frames) == 0 {
			return lir.Datum{}, reject.Runtimef("exec: expected exactly one row, got none")
		}
		if len(frames) > 1 {
			return lir.Datum{}, reject.Runtimef("exec: expected exactly one row, got more")
		}
		return frameToObject(out, frames[0]), nil
	case lir.CardScalar:
		if len(frames) == 0 {
			return lir.NullDatum(), nil
		}
		return frameScalar(out, frames[0]), nil
	default:
		return framesToArray(out, frames), nil
	}
}
