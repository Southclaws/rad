package rad

import (
	"context"
	"fmt"

	"github.com/Southclaws/rad/clients/go/api/oas"
)

// SchemaTransitionListOption filters a transition-list snapshot.
type SchemaTransitionListOption func(*oas.SchemaTransitionListParams)

// WithTransitionKind returns only transitions using kind's physical protocol.
func WithTransitionKind(kind TransitionKind) SchemaTransitionListOption {
	return func(params *oas.SchemaTransitionListParams) {
		params.Kind = oas.NewOptTransitionKind(oas.TransitionKind(kind))
	}
}

// WithTransitionState returns only transitions in state.
func WithTransitionState(state TransitionState) SchemaTransitionListOption {
	return func(params *oas.SchemaTransitionListParams) {
		params.State = oas.NewOptTransitionState(oas.TransitionState(state))
	}
}

// SchemaTransitions returns a coherent administrative snapshot of durable
// online schema work. It does not claim or advance a transition worker.
func (c *Client) SchemaTransitions(
	ctx context.Context,
	options ...SchemaTransitionListOption,
) ([]TransitionControl, error) {
	if err := c.ensureSchema(ctx); err != nil {
		return nil, err
	}
	var params oas.SchemaTransitionListParams
	for _, option := range options {
		option(&params)
	}
	response, err := c.oas.SchemaTransitionList(ctx, params)
	if err != nil {
		return nil, transportError(err)
	}
	out := make([]TransitionControl, len(response.Transitions))
	for i := range response.Transitions {
		out[i], err = transitionControlFromOAS(response.Transitions[i])
		if err != nil {
			return nil, err
		}
	}
	return out, nil
}

// SchemaTransition returns the current administrative view of one durable
// online schema transition.
func (c *Client) SchemaTransition(ctx context.Context, transitionID string) (TransitionControl, error) {
	if err := c.ensureSchema(ctx); err != nil {
		return TransitionControl{}, err
	}
	response, err := c.oas.SchemaTransitionGet(ctx, oas.SchemaTransitionGetParams{Transition: transitionID})
	if err != nil {
		return TransitionControl{}, transportError(err)
	}
	switch value := response.(type) {
	case *oas.TransitionControl:
		return transitionControlFromOAS(*value)
	case *oas.Problem:
		return TransitionControl{}, apiError(*value)
	default:
		return TransitionControl{}, fmt.Errorf("rad: unexpected schema transition response %T", response)
	}
}

// CancelSchemaTransition atomically cancels one durable transition. It is an
// administrative operation and cannot be interleaved with PIR statements.
func (c *Client) CancelSchemaTransition(ctx context.Context, transitionID string) (TransitionControl, error) {
	if err := c.ensureSchema(ctx); err != nil {
		return TransitionControl{}, err
	}
	response, err := c.oas.SchemaTransitionCancel(ctx, oas.SchemaTransitionCancelParams{Transition: transitionID})
	if err != nil {
		return TransitionControl{}, transportError(err)
	}
	switch value := response.(type) {
	case *oas.TransitionControl:
		return transitionControlFromOAS(*value)
	case *oas.SchemaTransitionCancelNotFound:
		return TransitionControl{}, apiError(oas.Problem(*value))
	case *oas.SchemaTransitionCancelConflict:
		return TransitionControl{}, apiError(oas.Problem(*value))
	case *oas.SchemaTransitionCancelUnprocessableEntity:
		return TransitionControl{}, apiError(oas.Problem(*value))
	default:
		return TransitionControl{}, fmt.Errorf("rad: unexpected schema transition cancellation response %T", response)
	}
}

func transitionControlFromOAS(control oas.TransitionControl) (TransitionControl, error) {
	if control.Generation < 0 || control.RowsScanned < 0 || control.AppliedDelta < 0 || control.DeltaLag < 0 {
		return TransitionControl{}, fmt.Errorf("rad: transition %q contains negative progress", control.TransitionID)
	}
	return TransitionControl{
		Kind:              string(control.Kind),
		TransitionID:      control.TransitionID,
		ObjectID:          control.ObjectID,
		TransitionKind:    TransitionKind(control.TransitionKind),
		State:             TransitionState(control.State),
		Generation:        uint64(control.Generation),
		Prerequisites:     control.Prerequisites,
		RetainedWorkState: TransitionWorkState(control.RetainedWorkState),
		LastError:         control.LastError.Or(""),
		RowsScanned:       uint64(control.RowsScanned),
		AppliedDelta:      uint64(control.AppliedDelta),
		DeltaLag:          uint64(control.DeltaLag),
	}, nil
}
