package rad

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/Southclaws/rad/clients/go/protocol"
)

func TestReadOnlyProblemRetainsForbiddenStatusAndReason(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		response.Header().Set("Content-Type", "application/problem+json")
		response.WriteHeader(http.StatusForbidden)
		_ = json.NewEncoder(response).Encode(protocol.NewProblem(
			protocol.CodeInvalid,
			http.StatusForbidden,
			"database is read-only",
		).WithReason("read_only"))
	}))
	defer server.Close()

	client, err := Dial("rad://" + strings.TrimPrefix(server.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	err = client.TableDelete(context.Background(), "users")
	var apiErr *APIError
	if !errors.As(err, &apiErr) {
		t.Fatalf("error = %T %v, want *APIError", err, err)
	}
	if apiErr.Problem.Status != http.StatusForbidden {
		t.Fatalf("status = %d, want 403", apiErr.Problem.Status)
	}
	if apiErr.Problem.Code != protocol.CodeInvalid || apiErr.Problem.Reason != "read_only" {
		t.Fatalf("problem = %#v, want invalid/read_only", apiErr.Problem)
	}
}
