package api

import (
	"context"
	"errors"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol"
)

func TestSchemaCompatibility(t *testing.T) {
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, testSchema); err != nil {
		t.Fatal(err)
	}
	server, err := client.Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if err := client.CheckSchema(ctx, server.SchemaVersion, server.SchemaHash); err != nil {
		t.Fatalf("exact schema identity rejected: %v", err)
	}
	diff, err := client.SchemaDiff(ctx, testSchema)
	if err != nil {
		t.Fatalf("no-op schema diff: %v", err)
	}
	program, ok := diff.Program.(map[string]any)
	if !ok {
		t.Fatalf("no-op program = %T", diff.Program)
	}
	statements, ok := program["statements"].([]any)
	if !ok || len(statements) != 0 {
		t.Fatalf("no-op statements = %#v", program["statements"])
	}

	assertCompatibilityReason(t, client.CheckSchema(ctx, server.SchemaVersion-1, server.SchemaHash), "schema_client_outdated")
	assertCompatibilityReason(t, client.CheckSchema(ctx, server.SchemaVersion+1, server.SchemaHash), "schema_server_outdated")
	assertCompatibilityReason(t, client.CheckSchema(ctx, server.SchemaVersion, "sha256:different"), "schema_history_diverged")
}

func assertCompatibilityReason(t *testing.T, err error, want string) {
	t.Helper()
	var apiError *radclient.APIError
	if !errors.As(err, &apiError) {
		t.Fatalf("error = %v, want APIError", err)
	}
	if apiError.Problem.Reason != want {
		t.Fatalf("reason = %q, want %q", apiError.Problem.Reason, want)
	}
}

func TestSchemaMigrateRejectsStalePreflight(t *testing.T) {
	client := testServer(t)
	ctx := context.Background()
	stale, err := client.SchemaDiff(ctx, testSchema)
	if err != nil {
		t.Fatal(err)
	}
	other := `
tables:
  - id: 1
    name: notes
    columns:
      - { id: 1, name: id, type: int64, pk: true }
`
	if _, err := migrateSchema(ctx, client, other); err != nil {
		t.Fatal(err)
	}
	_, err = client.SchemaMigrate(ctx, testSchema, protocol.SchemaIdentity{
		SchemaVersion: stale.CurrentVersion, SchemaHash: stale.CurrentHash,
	}, false)
	assertCompatibilityReason(t, err, "serializable_conflict")
	server, err := client.Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if server.SchemaVersion != 1 {
		t.Fatalf("stale migration changed schema version to %d", server.SchemaVersion)
	}
}
