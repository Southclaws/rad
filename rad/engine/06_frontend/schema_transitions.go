package frontend

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// SchemaTransition returns the durable administrative view of one online
// schema transition without claiming or advancing its worker.
func (db *DB) SchemaTransition(ctx context.Context, transitionID string) (model.TransitionControl, error) {
	transition, ok, err := db.cat.GetTransition(ctx, transitionID)
	if err != nil {
		return model.TransitionControl{}, err
	}
	if !ok {
		return model.TransitionControl{}, reject.Fail(
			reject.ReasonNotFound,
			"catalog: transition %q does not exist",
			transitionID,
		)
	}
	return transition.Control(), nil
}

// SchemaTransitions returns a coherent snapshot of the durable administrative
// view of every online schema transition.
func (db *DB) SchemaTransitions(ctx context.Context) ([]model.TransitionControl, error) {
	transitions, err := db.cat.ListTransitions(ctx)
	if err != nil {
		return nil, err
	}
	controls := make([]model.TransitionControl, len(transitions))
	for i := range transitions {
		controls[i] = transitions[i].Control()
	}
	return controls, nil
}

// CancelSchemaTransition atomically invalidates a transition's worker and
// removes its foreground write obligations. It is an administrative operation,
// not a statement that can be interleaved with a PIR program.
func (db *DB) CancelSchemaTransition(ctx context.Context, transitionID string) (model.TransitionControl, error) {
	transition, err := db.eng.CancelSchemaTransition(ctx, transitionID)
	if err != nil {
		return model.TransitionControl{}, err
	}
	return transition.Control(), nil
}
