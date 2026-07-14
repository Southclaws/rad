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
// with json.Number, so int64 keeps full precision) and the per-statement
// summary in execution order.
type ProgramResult struct {
	Result     any
	Statements []StatementResult
}

// Execute runs a program and returns its result.
func (c *Client) Execute(ctx context.Context, prog protocol.Program) (ProgramResult, error) {
	raw, err := protocol.MarshalProgram(prog)
	if err != nil {
		return ProgramResult{}, err
	}
	res, err := c.oas.Execute(ctx, oas.Program(raw))
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
