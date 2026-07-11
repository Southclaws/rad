package exec

// Execute: the relation-graph read entry point. Bind against the catalog,
// plan, run the operator tree, and materialise the root according to its
// cardinality. Every statement binds and executes against one KV view —
// Engine.Execute takes a snapshot for the statement's duration, Tx.Execute
// uses the transaction's snapshot plus its own writes — so schema
// resolution and data reads can never observe different moments.

import (
	"context"
	"fmt"

	kv "rad/rad/01_kv"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
	planner "rad/rad/04_planner"
)

// Execute runs an unbound query against a snapshot of committed state.
func (e *Engine) Execute(ctx context.Context, q qir.Query) (qir.Datum, error) {
	return e.executeSnapshot(ctx, q, false)
}

// Execute inside a transaction sees its snapshot plus its own writes. The
// schema lookups join the transaction's read set, so concurrent DDL on a
// table the statement touched conflicts at commit.
func (tx *Tx) Execute(ctx context.Context, q qir.Query) (qir.Datum, error) {
	return tx.e.execute(ctx, tx.txn, q, false)
}

// ExecuteNested runs with keyed batching disabled — every correlated
// crossing evaluates per row. Results are identical to Execute by
// construction; the conformance suite holds the executor to it.
func (e *Engine) ExecuteNested(ctx context.Context, q qir.Query) (qir.Datum, error) {
	return e.executeSnapshot(ctx, q, true)
}

// executeSnapshot gives an autocommit read one statement-scoped snapshot;
// read-only, so it is discarded rather than committed.
func (e *Engine) executeSnapshot(ctx context.Context, q qir.Query, forceNested bool) (qir.Datum, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return qir.Datum{}, err
	}
	defer txn.Rollback()
	return e.execute(ctx, txn, q, forceNested)
}

func (e *Engine) execute(ctx context.Context, view kv.KV, q qir.Query, forceNested bool) (qir.Datum, error) {
	bq, err := planner.Bind(ctx, catalog.NewReader(view), q)
	if err != nil {
		return qir.Datum{}, err
	}
	pp := planner.PlanQuery(bq)

	ex := newExecutor(view)
	ex.forceNested = forceNested
	op, err := ex.build(ctx, pp.Root, bound.Env{})
	if err != nil {
		return qir.Datum{}, err
	}
	defer op.Close()

	switch pp.Card {
	case qir.CardFirst:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return qir.Datum{}, err
		}
		if !ok {
			return qir.NullDatum(), nil
		}
		return frameToObject(pp.Out, f), nil

	case qir.CardExactlyOne:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return qir.Datum{}, err
		}
		if !ok {
			return qir.Datum{}, fmt.Errorf("exec: expected exactly one row, got none")
		}
		if _, more, err := op.Next(ctx); err != nil {
			return qir.Datum{}, err
		} else if more {
			return qir.Datum{}, fmt.Errorf("exec: expected exactly one row, got more")
		}
		return frameToObject(pp.Out, f), nil

	case qir.CardScalar:
		f, ok, err := op.Next(ctx)
		if err != nil {
			return qir.Datum{}, err
		}
		fld := pp.Out.Fields[0]
		if !ok {
			return qir.NullDatum(), nil
		}
		if fld.Type.Kind.Scalar() {
			return qir.ScalarDatum(f.vals[fld.Slot]), nil
		}
		if d, hasNested := f.nested[fld.Slot]; hasNested {
			return d, nil
		}
		return qir.NullDatum(), nil

	default: // many
		frames, err := drainOp(ctx, op)
		if err != nil {
			return qir.Datum{}, err
		}
		elems := make([]qir.Datum, len(frames))
		for i, f := range frames {
			elems[i] = frameToObject(pp.Out, f)
		}
		return qir.ArrayDatum(elems), nil
	}
}
