package radclient

// Execute runs a PIR program over /execute and returns the result statement's
// datum plus the per-statement summary. This is the general data-plane entry;
// Query is a one-statement read program.

import (
	"context"
	"fmt"

	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/protocol"
)

// StatementResult is one statement's lightweight outcome.
type StatementResult struct {
	Name     string
	Affected int
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
func (c *Client) Execute(ctx context.Context, prog protocol.Program, opts ...ExecuteOption) (ProgramResult, error) {
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
		return ProgramResult{}, apiError(oas.Problem(*v))
	}
	return ProgramResult{}, fmt.Errorf("rad: unexpected execute response %T", res)
}
