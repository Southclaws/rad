package exec

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// activateWaitingSchemaTransition atomically publishes the foreground
// obligations of a transition whose prerequisites are now ready. It returns
// the unchanged waiting record while any prerequisite is still active.
func (e *Engine) activateWaitingSchemaTransition(
	ctx context.Context,
	transitionID string,
) (model.SchemaTransition, error) {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	defer txn.Rollback()
	var transition model.SchemaTransition
	_, err = change.Apply(ctx, txn, func(mutation *change.Mutation) error {
		var err error
		transition, err = mutation.ActivateWaitingTransition(ctx, transitionID)
		return err
	})
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if err := txn.Commit(ctx); err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.State != model.TransitionWaiting {
		e.kickSchemaJobs()
	}
	return transition, nil
}
