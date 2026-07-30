package rad

// Execute runs a PIR program over /execute and returns the result statement's
// datum plus the per-statement summary. This is the general data-plane entry;
// Query is a one-statement read program.

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"

	"github.com/Southclaws/rad/clients/go/api/oas"
	"github.com/Southclaws/rad/clients/go/protocol"
	"github.com/Southclaws/rad/clients/go/protocol/pirwire"
)

// StatementResult is one statement's lightweight outcome.
type StatementResult struct {
	Name     string
	Affected int
	Control  *TransitionControl
}

// TransitionKind selects the physical protocol used for online schema work.
type TransitionKind string

const (
	TransitionIndexBuild           TransitionKind = "index_build"
	TransitionColumnReplacement    TransitionKind = "column_replacement"
	TransitionConstraintValidation TransitionKind = "constraint_validation"
)

// TransitionState is the durable lifecycle state of online schema work.
type TransitionState string

const (
	TransitionWaiting    TransitionState = "waiting"
	TransitionBuilding   TransitionState = "building"
	TransitionCatchingUp TransitionState = "catching_up"
	TransitionValidating TransitionState = "validating"
	TransitionReady      TransitionState = "ready"
	TransitionFailed     TransitionState = "failed"
	TransitionCancelled  TransitionState = "cancelled"
)

// TransitionWorkState summarizes advisory retained-work pressure.
type TransitionWorkState string

const (
	TransitionWorkNormal     TransitionWorkState = "normal"
	TransitionWorkDegraded   TransitionWorkState = "degraded"
	TransitionWorkWriteGated TransitionWorkState = "write_gated"
)

// TransitionControl is the durable identity, lifecycle state, and dependency
// set returned by transition-start statements and administrative transition
// endpoints. Retained-work and progress fields are advisory observations;
// correctness must depend only on normative state.
type TransitionControl struct {
	Kind              string              `json:"kind"`
	TransitionID      string              `json:"transition_id"`
	ObjectID          string              `json:"object_id"`
	TransitionKind    TransitionKind      `json:"transition_kind"`
	State             TransitionState     `json:"state"`
	Generation        uint64              `json:"generation"`
	Prerequisites     []string            `json:"prerequisites"`
	RetainedWorkState TransitionWorkState `json:"retained_work_state,omitempty"`
	LastError         string              `json:"last_error,omitempty"`
	RowsScanned       uint64              `json:"rows_scanned,omitempty"`
	AppliedDelta      uint64              `json:"applied_delta,omitempty"`
	DeltaLag          uint64              `json:"delta_lag,omitempty"`
}

// ProgramResult is a program's outcome: the result statement's datum (decoded
// with json.Number, so int64 keeps full precision), the per-statement summary
// in execution order, and — when the query plan was requested — the plan as
// decoded free-form JSON.
type ProgramResult struct {
	Result     any
	Statements []StatementResult
	Plan       any // present only when Execute was called WithPlan; free-form
}

// ExecuteOption sets a transport knob on Execute (query params, not IR).
type ExecuteOption func(*oas.ExecuteParams)

// WithPlan asks the server to return the query plan for each statement.
func WithPlan() ExecuteOption {
	return func(p *oas.ExecuteParams) { p.ShowPlan = oas.NewOptBool(true) }
}

// DryRun asks the server to bind and plan but execute nothing (no result).
func DryRun() ExecuteOption {
	return func(p *oas.ExecuteParams) { p.DryRun = oas.NewOptBool(true) }
}

// Execute runs a program and returns its result. Options attach the query plan
// (WithPlan) and/or skip execution (DryRun).
func (c *Client) Execute(ctx context.Context, prog pirwire.Program, opts ...ExecuteOption) (ProgramResult, error) {
	if err := c.ensureSchema(ctx); err != nil {
		return ProgramResult{}, err
	}
	raw, err := protocol.MarshalProgram(prog)
	if err != nil {
		return ProgramResult{}, err
	}
	var params oas.ExecuteParams
	for _, o := range opts {
		o(&params)
	}
	res, err := c.oas.Execute(ctx, oas.Program(raw), params)
	if err != nil {
		return ProgramResult{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.ProgramResult:
		datum, err := decodeResult(v.Result)
		if err != nil {
			return ProgramResult{}, err
		}
		out := ProgramResult{Result: datum, Statements: make([]StatementResult, len(v.Statements))}
		for i, s := range v.Statements {
			out.Statements[i] = StatementResult{Name: s.Name, Affected: s.Affected}
			if raw := []byte(s.Control); len(raw) > 0 && !bytes.Equal(raw, []byte("null")) {
				var control TransitionControl
				if err := json.Unmarshal(raw, &control); err != nil {
					return ProgramResult{}, fmt.Errorf("rad: decode statement %q control result: %w", s.Name, err)
				}
				out.Statements[i].Control = &control
			}
		}
		if plan, err := decodeResult(v.Plan); err == nil {
			out.Plan = plan // nil when the server sent JSON null (no plan requested)
		}
		return out, nil
	case *oas.ExecuteBadRequest:
		return ProgramResult{}, apiError(oas.Problem(*v))
	case *oas.ExecuteConflict:
		return ProgramResult{}, apiError(oas.Problem(*v))
	case *oas.ExecuteUnprocessableEntity:
		c.schema.invalidate()
		return ProgramResult{}, apiError(oas.Problem(*v))
	}
	return ProgramResult{}, fmt.Errorf("rad: unexpected execute response %T", res)
}
