package api

// The /execute endpoint: run a PIR program. The raw body is validated and
// decoded by protocol.UnmarshalProgram (envelope plus each statement's LIR
// relation against the LIR schema), each statement's relation is materialised
// into the engine's unbound IR with the existing graphQuery, and the whole
// program runs as one atomic transaction. A decode/validation failure is a
// 400; a bind or execution failure is a 422 (or 409 for a serializable race),
// classified by the engine's typed errors exactly as /query is.

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/protocol"
)

func (a *dbAPI) Execute(ctx context.Context, req oas.Program) (oas.ExecuteRes, error) {
	prog, err := protocol.UnmarshalProgram(req)
	if err != nil {
		op := api.ProblemToOAS(protocol.NewProblem(protocol.CodeInvalid, http.StatusBadRequest, err.Error()))
		return (*oas.ExecuteBadRequest)(&op), nil
	}

	ep, err := programToEngine(prog)
	if err == nil {
		var res exec.ProgramResult
		if res, err = a.db.ExecuteProgram(ctx, ep); err == nil {
			return programResult(res)
		}
	}
	if p := clientProblem(err); p != nil {
		op := api.ProblemToOAS(*p)
		if p.Code == protocol.CodeConflict {
			return (*oas.ExecuteConflict)(&op), nil
		}
		return (*oas.ExecuteUnprocessableEntity)(&op), nil
	}
	return nil, err
}

// programToEngine materialises each statement's wire relation into the
// engine's unbound IR, mirroring how /query lowers a single relation. Each
// statement's refs may resolve to any earlier statement's result, so the set
// of prior names accumulates as external bindings — which also enforces the
// backward-only rule, since a forward reference is not yet in the set.
func programToEngine(p protocol.Program) (exec.Program, error) {
	stmts := make([]exec.ProgramStatement, len(p.Statements))
	external := map[string]bool{}
	for i, s := range p.Statements {
		rel, err := graphQueryExternal(s.Relation, external)
		if err != nil {
			return exec.Program{}, fmt.Errorf("statement %q: %w", s.Name, err)
		}
		stmts[i] = exec.ProgramStatement{
			Name:  s.Name,
			Kind:  exec.StatementKind(s.Kind),
			Table: s.Table,
			Rel:   rel,
		}
		external[s.Name] = true
	}
	return exec.Program{Statements: stmts, Result: p.Result}, nil
}

// programResult shapes a program outcome into the wire response: the result
// datum as raw JSON plus the per-statement summary.
func programResult(res exec.ProgramResult) (oas.ExecuteRes, error) {
	raw, err := json.Marshal(frontend.DatumJSON(res.Result))
	if err != nil {
		return nil, fmt.Errorf("encode result datum: %w", err)
	}
	stmts := make([]oas.StatementResult, len(res.Statements))
	for i, s := range res.Statements {
		stmts[i] = oas.StatementResult{Name: s.Name, Affected: s.Affected}
	}
	return &oas.ProgramResult{Result: oas.Value(raw), Statements: stmts}, nil
}
