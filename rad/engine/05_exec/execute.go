package exec

// Execute: the relation-graph read entry point. Bind against the catalog,
// plan, run the operator tree, and materialise the root according to its
// cardinality. Every statement binds and executes against one KV view —
// Engine.Execute takes a snapshot for the statement's duration, Tx.Execute
// uses the transaction's snapshot plus its own writes — so schema
// resolution and data reads can never observe different moments.

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
	"github.com/Southclaws/rad/rad/engine/05_exec/query"
)

// Execute runs an unbound query against a snapshot of committed state.
func (e *Engine) Execute(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return e.executeSnapshot(ctx, q, false)
}

// Execute inside a transaction sees its snapshot plus its own writes. Binding
// produces an exact catalog dependency manifest; execution admits those typed
// fences into the transaction so only an incompatible concurrent catalog
// change conflicts at commit.
func (tx *Tx) Execute(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return tx.e.execute(ctx, tx.txn, txCatalog{tx: tx}, q, false)
}

// ExecuteForced runs a query with every access forced to a full table scan, so
// the residual filter — never an index's selection — decides which rows a query
// returns. By the planner's central invariant it must produce the same result
// as Execute; a differential that runs both is how that invariant is checked.
func (e *Engine) ExecuteForced(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return e.executeSnapshot(ctx, q, false, planner.FullScanOnly())
}

// ExecuteNested runs with keyed batching disabled — every correlated
// crossing evaluates per row. Results are identical to Execute by
// construction; the conformance suite holds the executor to it.
func (e *Engine) ExecuteNested(ctx context.Context, q lir.Query) (lir.Datum, error) {
	return e.executeSnapshot(ctx, q, true)
}

// executeSnapshot gives an autocommit read one statement-scoped snapshot;
// read-only, so it is discarded rather than committed.
func (e *Engine) executeSnapshot(ctx context.Context, q lir.Query, forceNested bool, planOpts ...planner.Option) (lir.Datum, error) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return lir.Datum{}, err
	}
	defer txn.Rollback()
	return e.execute(ctx, txn, store.New(txn), q, forceNested, planOpts...)
}

func (e *Engine) execute(ctx context.Context, view kv.KV, catalog binder.Catalog, q lir.Query, forceNested bool, planOpts ...planner.Option) (lir.Datum, error) {
	bq, err := binder.Bind(ctx, catalog, q)
	if err != nil {
		return lir.Datum{}, err
	}
	e.yield(ctx, YieldBindingResolved, "query")
	pp := planner.PlanQuery(bq, planOpts...)

	runner := query.New(view, query.Limits{MaxIterations: e.recur.MaxIterations, MaxRows: e.recur.MaxRows})
	runner.SetForceNested(forceNested)
	result, err := runner.Execute(ctx, pp)
	if err == nil {
		e.yield(ctx, YieldDependencyFencesAdmitted, "query")
	}
	return result, err
}
