package api

import (
	"context"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol"
)

func schemaProgramStatements(t *testing.T, diff protocol.SchemaDiff) []any {
	t.Helper()
	program, ok := diff.Program.(map[string]any)
	if !ok {
		t.Fatalf("schema program = %T, want object", diff.Program)
	}
	statements, ok := program["statements"].([]any)
	if !ok {
		t.Fatalf("schema program statements = %#v, want array", program["statements"])
	}
	return statements
}

func migrateSchema(ctx context.Context, client *radclient.Client, source string) (protocol.SchemaMigration, error) {
	diff, err := client.SchemaDiff(ctx, source)
	if err != nil {
		return protocol.SchemaMigration{}, err
	}
	return client.SchemaMigrate(ctx, source, protocol.SchemaIdentity{
		SchemaVersion: diff.CurrentVersion, SchemaHash: diff.CurrentHash,
	}, false)
}
