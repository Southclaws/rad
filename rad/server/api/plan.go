package api

import (
	"context"
	"encoding/json"
	"io"
	"net/http"

	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	"github.com/Southclaws/rad/rad/protocol"
)

// Explain binds and plans a raw LIR query document and renders the physical
// plan as text. It reuses the exact wire-to-IR and binding path a real query
// takes, but stops before execution: nothing is read from data storage and no
// rows are produced. Every failure — malformed JSON, an invalid graph, an
// unknown table or column — is the caller's, and carries a describing message.
func Explain(ctx context.Context, cat planner.Catalog, raw []byte) (string, error) {
	wire, err := protocol.UnmarshalQuery(raw)
	if err != nil {
		return "", err
	}
	q, err := graphQuery(wire)
	if err != nil {
		return "", err
	}
	bound, err := planner.Bind(ctx, cat, q)
	if err != nil {
		return "", err
	}
	return planner.PrintPlan(planner.PlanQuery(bound)), nil
}

// PlanHandler serves POST /api/plan for the admin UI's plan viewer: the request
// body is a LIR query document and the response is `{"plan": "..."}`. It is a
// supplemental admin endpoint, deliberately outside the OpenAPI contract. Any
// planning failure is reported as `{"error": "..."}` so the UI can surface it
// while keeping the last valid plan on screen.
func PlanHandler(cat planner.Catalog) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(io.LimitReader(r.Body, 1<<20))
		if err != nil {
			writePlanJSON(w, http.StatusBadRequest, map[string]string{"error": err.Error()})
			return
		}
		plan, err := Explain(r.Context(), cat, body)
		if err != nil {
			writePlanJSON(w, http.StatusUnprocessableEntity, map[string]string{"error": err.Error()})
			return
		}
		writePlanJSON(w, http.StatusOK, map[string]string{"plan": plan})
	}
}

func writePlanJSON(w http.ResponseWriter, code int, body map[string]string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(body)
}
