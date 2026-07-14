package api

import (
	"context"
	"encoding/json"
	"errors"
	"log"
	"net/http"

	"github.com/ogen-go/ogen/ogenerrors"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/engine/reject"
	"github.com/Southclaws/rad/rad/protocol"
)

// clientProblem classifies an engine or conversion error as a client-facing
// problem, or returns nil when it is an unexpected internal error that should
// surface as a 500 through NewError. Classification is by error type, marked
// at the source (rad/engine/reject) — never by message text: input errors
// are the caller's fault at request time, runtime errors are data-dependent
// failures of a valid request, conflicts are retryable optimistic races.
// Anything unmarked is internal and stays hidden.
func clientProblem(err error) *protocol.Problem {
	var we wireErr
	switch {
	case errors.As(err, &we):
		p := protocol.NewProblem(protocol.CodeInvalid, http.StatusUnprocessableEntity, err.Error())
		return &p
	case frontend.IsConflict(err):
		p := protocol.NewProblem(protocol.CodeConflict, http.StatusConflict, err.Error())
		return &p
	case reject.IsInput(err):
		p := protocol.NewProblem(protocol.CodeInvalid, http.StatusUnprocessableEntity, err.Error())
		return &p
	case reject.IsRuntime(err):
		p := protocol.NewProblem(protocol.CodeExecutionFailed, http.StatusUnprocessableEntity, err.Error())
		return &p
	default:
		return nil
	}
}

// NewError renders an unexpected internal error as the contract's default
// response. The detail is deliberately generic; the real error is logged.
func (a *dbAPI) NewError(ctx context.Context, err error) *oas.InternalServerErrorStatusCode {
	log.Printf("internal error: %v", err)
	return &oas.InternalServerErrorStatusCode{
		StatusCode: http.StatusInternalServerError,
		Response:   api.ProblemToOAS(protocol.NewProblem(protocol.CodeInternal, http.StatusInternalServerError, "internal error")),
	}
}

// problemErrorHandler renders request decoding and routing failures raised by
// the generated server as RFC 7807 problems, so even malformed requests get a
// contract-shaped body rather than ogen's plain text default.
func problemErrorHandler(ctx context.Context, w http.ResponseWriter, r *http.Request, err error) {
	status := ogenerrors.ErrorCode(err)
	w.Header().Set("Content-Type", api.ProblemContentType)
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(protocol.NewProblem(protocol.CodeInvalid, status, err.Error()))
}

func rowJSON(row lir.Row) protocol.Record {
	if row == nil {
		return nil
	}
	return frontend.RowJSON(row)
}
