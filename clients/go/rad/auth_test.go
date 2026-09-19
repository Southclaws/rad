package rad

import (
	"context"
	"encoding/json"
	"net/http"
	"testing"

	"github.com/Southclaws/rad/clients/go/protocol"
)

func TestWithBearerTokenAddsTheAuthorizationHeader(t *testing.T) {
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if value := request.Header.Get("Authorization"); value != "Bearer access-token" {
			t.Errorf("authorization = %q, want bearer access token", value)
		}
		response.WriteHeader(http.StatusNoContent)
	}), WithBearerToken("access-token"))

	if err := client.TableDelete(context.Background(), "users"); err != nil {
		t.Fatal(err)
	}
}

func TestUnauthenticatedProblemDecodesAsAPIError(t *testing.T) {
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		response.Header().Set("Content-Type", "application/problem+json")
		response.Header().Set("WWW-Authenticate", `Bearer realm="rad", error="invalid_token"`)
		response.WriteHeader(http.StatusUnauthorized)
		_ = json.NewEncoder(response).Encode(protocol.NewProblem(
			protocol.CodeUnauthenticated,
			http.StatusUnauthorized,
			"The bearer access token is invalid.",
		).WithReason("invalid_token"))
	}))

	err := client.TableDelete(context.Background(), "users")
	if !IsUnauthenticated(err) {
		t.Fatalf("error = %T %v, want unauthenticated API error", err, err)
	}
}

func TestForbiddenProblemDecodesAsAPIError(t *testing.T) {
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		response.Header().Set("Content-Type", "application/problem+json")
		response.Header().Set("WWW-Authenticate", `Bearer realm="rad", error="insufficient_scope"`)
		response.WriteHeader(http.StatusForbidden)
		_ = json.NewEncoder(response).Encode(protocol.NewProblem(
			protocol.CodeForbidden,
			http.StatusForbidden,
			"The access token does not grant catalog access.",
		).WithReason("insufficient_scope"))
	}))

	err := client.TableDelete(context.Background(), "users")
	if !IsForbidden(err) {
		t.Fatalf("error = %T %v, want forbidden API error", err, err)
	}
}
