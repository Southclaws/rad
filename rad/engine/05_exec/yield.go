package exec

import "context"

// YieldPoint identifies a semantic concurrency boundary. Hooks are inert
// unless an Engine is constructed with WithYieldHook; tests use them to hold
// operations at meaningful boundaries instead of relying on sleeps.
type YieldPoint string

const (
	YieldCatalogPinned              YieldPoint = "catalog_pinned"
	YieldSnapshotBegun              YieldPoint = "snapshot_begun"
	YieldBindingResolved            YieldPoint = "binding_resolved"
	YieldDependencyFencesAdmitted   YieldPoint = "dependency_fences_admitted"
	YieldCatalogPublicationPrepared YieldPoint = "catalog_publication_prepared"
	YieldCommitReady                YieldPoint = "commit_ready"
	YieldTransactionCommitted       YieldPoint = "transaction_committed"
	YieldOwnerTakeover              YieldPoint = "owner_takeover"
	YieldTransitionBatchIntent      YieldPoint = "transition_batch_intent"
	YieldTransitionCheckpoint       YieldPoint = "transition_checkpoint"
	YieldFinalizationGateAcquired   YieldPoint = "finalization_gate_acquired"
)

// YieldEvent describes one reached semantic boundary. Actor is optional and
// comes from WithYieldActor; Entity is the stable table, transition, or owner
// identity associated with the boundary when one exists.
type YieldEvent struct {
	Point  YieldPoint
	Actor  string
	Entity string
}

// YieldHook runs synchronously at semantic boundaries. A hook may block to
// force a test schedule, but must return when ctx is cancelled. It has no
// error result because boundaries after a durable commit are observational:
// a test hook must never turn a successful commit into an apparent failure.
type YieldHook func(context.Context, YieldEvent)

type yieldActorKey struct{}

// WithYieldActor labels direct engine work for deterministic concurrency
// tests. The label affects only YieldEvent diagnostics and never execution.
func WithYieldActor(ctx context.Context, actor string) context.Context {
	return context.WithValue(ctx, yieldActorKey{}, actor)
}

func (e *Engine) yield(ctx context.Context, point YieldPoint, entity string) {
	if e.yieldHook == nil {
		return
	}
	actor, _ := ctx.Value(yieldActorKey{}).(string)
	e.yieldHook(ctx, YieldEvent{Point: point, Actor: actor, Entity: entity})
}
