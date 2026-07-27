package api

import (
	"context"
	"fmt"
	"math"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
)

func (a *dbAPI) SchemaTransitionList(
	ctx context.Context,
	params oas.SchemaTransitionListParams,
) (*oas.TransitionList, error) {
	controls, err := a.db.SchemaTransitions(ctx)
	if err != nil {
		return nil, err
	}
	kind, filterKind := params.Kind.Get()
	state, filterState := params.State.Get()
	out := &oas.TransitionList{Transitions: make([]oas.TransitionControl, 0, len(controls))}
	for _, control := range controls {
		if filterKind && string(control.TransitionKind) != string(kind) {
			continue
		}
		if filterState && string(control.State) != string(state) {
			continue
		}
		converted, err := transitionControlOAS(control)
		if err != nil {
			return nil, err
		}
		out.Transitions = append(out.Transitions, converted)
	}
	return out, nil
}

func (a *dbAPI) SchemaTransitionGet(
	ctx context.Context,
	params oas.SchemaTransitionGetParams,
) (oas.SchemaTransitionGetRes, error) {
	control, err := a.db.SchemaTransition(ctx, params.Transition)
	if err == nil {
		out, err := transitionControlOAS(control)
		return &out, err
	}
	if problem := clientProblem(err); problem != nil && problem.Code == protocol.CodeNotFound {
		out := api.ProblemToOAS(*problem)
		return &out, nil
	}
	return nil, err
}

func (a *dbAPI) SchemaTransitionCancel(
	ctx context.Context,
	params oas.SchemaTransitionCancelParams,
) (oas.SchemaTransitionCancelRes, error) {
	control, err := a.db.CancelSchemaTransition(ctx, params.Transition)
	if err == nil {
		out, err := transitionControlOAS(control)
		return &out, err
	}
	problem := clientProblem(err)
	if problem == nil {
		return nil, err
	}
	out := api.ProblemToOAS(*problem)
	switch problem.Code {
	case protocol.CodeNotFound:
		return (*oas.SchemaTransitionCancelNotFound)(&out), nil
	case protocol.CodeConflict:
		return (*oas.SchemaTransitionCancelConflict)(&out), nil
	case protocol.CodeInvalid, protocol.CodeExecutionFailed:
		return (*oas.SchemaTransitionCancelUnprocessableEntity)(&out), nil
	default:
		return nil, err
	}
}

func transitionControlOAS(control model.TransitionControl) (oas.TransitionControl, error) {
	if control.Generation > math.MaxInt64 || control.RowsScanned > math.MaxInt64 ||
		control.AppliedDelta > math.MaxInt64 || control.DeltaLag > math.MaxInt64 {
		return oas.TransitionControl{}, fmt.Errorf(
			"catalog: transition %q progress exceeds the wire format",
			control.TransitionID,
		)
	}
	out := oas.TransitionControl{
		Kind:              oas.TransitionControlKindTransition,
		TransitionID:      control.TransitionID,
		ObjectID:          control.ObjectID,
		TransitionKind:    oas.TransitionKind(control.TransitionKind),
		State:             oas.TransitionState(control.State),
		Generation:        int64(control.Generation),
		Prerequisites:     append([]string{}, control.Prerequisites...),
		RetainedWorkState: oas.TransitionWorkState(control.RetainedWorkState),
		RowsScanned:       int64(control.RowsScanned),
		AppliedDelta:      int64(control.AppliedDelta),
		DeltaLag:          int64(control.DeltaLag),
	}
	if control.LastError != "" {
		out.LastError = oas.NewOptString(control.LastError)
	}
	if err := out.Validate(); err != nil {
		return oas.TransitionControl{}, fmt.Errorf(
			"catalog: invalid transition %q administrative state: %w",
			control.TransitionID,
			err,
		)
	}
	return out, nil
}
