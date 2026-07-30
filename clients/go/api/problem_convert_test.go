package api

import (
	"net/http"
	"testing"

	"github.com/Southclaws/rad/clients/go/api/oas"
	"github.com/Southclaws/rad/clients/go/protocol"
)

func TestProblemConversionUsesTheCodeDiscriminatedUnion(t *testing.T) {
	tests := []struct {
		code   string
		status int
		kind   oas.ProblemType
	}{
		{protocol.CodeInvalid, http.StatusUnprocessableEntity, oas.InvalidProblemProblem},
		{protocol.CodeExecutionFailed, http.StatusUnprocessableEntity, oas.ExecutionFailedProblemProblem},
		{protocol.CodeNotFound, http.StatusNotFound, oas.NotFoundProblemProblem},
		{protocol.CodeConflict, http.StatusConflict, oas.ConflictProblemProblem},
		{protocol.CodeInternal, http.StatusInternalServerError, oas.InternalProblemProblem},
	}
	for _, test := range tests {
		t.Run(test.code, func(t *testing.T) {
			problem := protocol.NewProblem(test.code, test.status, "detail").WithReason(test.code + "_reason")
			generated := ProblemToOAS(problem)
			if generated.Type != test.kind {
				t.Fatalf("union kind = %q, want %q", generated.Type, test.kind)
			}
			roundTrip := ProblemFromOAS(generated)
			if test.code == protocol.CodeInternal {
				if roundTrip.Reason != protocol.CodeInternal {
					t.Fatalf("internal reason = %q, want redacted %q", roundTrip.Reason, protocol.CodeInternal)
				}
				if roundTrip.Detail != "internal error" {
					t.Fatalf("internal detail = %q, want redacted detail", roundTrip.Detail)
				}
				return
			}
			if roundTrip != problem {
				t.Fatalf("round trip = %#v, want %#v", roundTrip, problem)
			}
		})
	}
}

func TestMalformedRequestProblemRetains400(t *testing.T) {
	problem := protocol.NewProblem(protocol.CodeInvalid, http.StatusBadRequest, "malformed JSON")
	generated := ProblemToOAS(problem)
	invalid, ok := generated.GetInvalidProblem()
	if !ok {
		t.Fatal("invalid problem did not select invalid union variant")
	}
	if invalid.Status != http.StatusBadRequest {
		t.Fatalf("status = %d, want 400", invalid.Status)
	}
}
