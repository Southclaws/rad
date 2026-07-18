package api

import (
	"net/http"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/protocol"
)

// schemaManagedProblem is the uniform rejection for every imperative catalog
// operation on a schema-managed database. Retrying cannot help because the
// mode is set once, so this is an invalid request rather than a conflict.
func schemaManagedProblem() oas.Problem {
	return api.ProblemToOAS(protocol.NewProblem(
		protocol.CodeInvalid, http.StatusUnprocessableEntity,
		"catalog: this database is schema-managed; direct catalog changes are disabled — apply changes through schema migration",
	))
}

// catalogProblem classifies a catalog mutation error as a client problem, or
// returns nil for an internal error.
func catalogProblem(err error) *oas.Problem {
	if p := clientProblem(err); p != nil {
		op := api.ProblemToOAS(*p)
		return &op
	}
	return nil
}
