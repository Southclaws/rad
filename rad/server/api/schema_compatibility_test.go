package api

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
)

func TestSchemaDiffWireDoesNotExposeTransitionHandles(t *testing.T) {
	srv := testHTTPServer(t)
	body, err := json.Marshal(map[string]string{"schema": testSchema})
	if err != nil {
		t.Fatal(err)
	}
	response, err := http.Post(srv.URL+"/schema/diff", "application/json", bytes.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		t.Fatalf("schema diff status = %d", response.StatusCode)
	}
	var result map[string]json.RawMessage
	if err := json.NewDecoder(response.Body).Decode(&result); err != nil {
		t.Fatal(err)
	}
	if _, exists := result["transition_ids"]; exists {
		t.Fatalf("advisory schema diff exposed transition handles: %s", result["transition_ids"])
	}
}

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
	if statements := schemaProgramStatements(t, diff); len(statements) != 0 {
		t.Fatalf("no-op statements = %#v", statements)
	}

	assertCompatibilityReason(t, client.CheckSchema(ctx, server.SchemaVersion-1, server.SchemaHash), "schema_client_outdated")
	assertCompatibilityReason(t, client.CheckSchema(ctx, server.SchemaVersion+1, server.SchemaHash), "schema_server_outdated")
	assertCompatibilityReason(t, client.CheckSchema(ctx, server.SchemaVersion, "sha256:different"), "schema_history_diverged")
}

func TestSchemaDiffEmitsChangeColumnDefaultPIR(t *testing.T) {
	client := testServer(t)
	ctx := context.Background()
	initial := `
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id,     type: int64, pk: true }
      - { id: 2, name: status, type: string, nullable: true, default: active }
`
	updated := `
tables:
  - id: 1
    name: items
    columns:
      - { id: 1, name: id,     type: int64, pk: true }
      - { id: 2, name: status, type: string, nullable: true, default: pending }
`
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	diff, err := client.SchemaDiff(ctx, updated)
	if err != nil {
		t.Fatal(err)
	}
	statements := schemaProgramStatements(t, diff)
	if len(statements) != 1 {
		t.Fatalf("statements = %#v", statements)
	}
	statement, ok := statements[0].(map[string]any)
	if !ok || statement["kind"] != "change_column_default" ||
		fmt.Sprint(statement["table_id"]) != "1" || fmt.Sprint(statement["column_id"]) != "2" {
		t.Fatalf("default statement = %#v", statements[0])
	}
	defaultValue, ok := statement["default"].(map[string]any)
	if !ok || defaultValue["kind"] != "literal" || defaultValue["value"] != "pending" {
		t.Fatalf("default payload = %#v", statement["default"])
	}
}

func TestSchemaDiffUsesCatalogMVCCTransitions(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`

	tests := []struct {
		name      string
		desired   string
		wantKind  string
		assertPIR func(*testing.T, map[string]any)
	}{
		{
			name: "physical type and format change",
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, format: unix_ms }
`,
			wantKind: "start_column_replacement",
			assertPIR: func(t *testing.T, statement map[string]any) {
				t.Helper()
				if fmt.Sprint(statement["table_id"]) != "1" || fmt.Sprint(statement["column_id"]) != "2" {
					t.Fatalf("replacement identity = %#v", statement)
				}
				replacement, ok := statement["replacement"].(map[string]any)
				if !ok || replacement["type"] != "int64" || replacement["nullable"] != true ||
					replacement["format"] != "unix_ms" || replacement["conversion"] != "strict_builtin" {
					t.Fatalf("replacement definition = %#v", statement["replacement"])
				}
			},
		},
		{
			name: "nullable tightening",
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`,
			wantKind: "start_constraint_validation",
			assertPIR: func(t *testing.T, statement map[string]any) {
				t.Helper()
				constraint, ok := statement["constraint"].(map[string]any)
				if !ok || constraint["kind"] != "not_null" || fmt.Sprint(constraint["column_id"]) != "2" || constraint["name"] == "" {
					t.Fatalf("constraint definition = %#v", statement["constraint"])
				}
			},
		},
		{
			name: "new index on existing table",
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
    indexes:
      - { name: events_value_idx, columns: [value] }
`,
			wantKind: "start_index_build",
			assertPIR: func(t *testing.T, statement map[string]any) {
				t.Helper()
				index, ok := statement["index"].(map[string]any)
				if !ok || index["name"] != "events_value_idx" {
					t.Fatalf("index definition = %#v", statement["index"])
				}
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			client := testServer(t)
			ctx := context.Background()
			if _, err := migrateSchema(ctx, client, initial); err != nil {
				t.Fatal(err)
			}
			diff, err := client.SchemaDiff(ctx, test.desired)
			if err != nil {
				t.Fatalf("schema diff: %v", err)
			}
			statements := schemaProgramStatements(t, diff)
			if len(statements) != 1 {
				t.Fatalf("statements = %#v, want one", statements)
			}
			statement, ok := statements[0].(map[string]any)
			if !ok || statement["kind"] != test.wantKind {
				t.Fatalf("statement = %#v, want kind %q", statements[0], test.wantKind)
			}
			test.assertPIR(t, statement)
		})
	}
}

func TestSchemaDiffFindsOnlineTransitionDataFailures(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": 1, "value": "not-an-integer"}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": 2, "value": nil}); err != nil {
		t.Fatal(err)
	}

	t.Run("strict conversion", func(t *testing.T) {
		diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true }
`)
		if err != nil {
			t.Fatal(err)
		}
		if len(diff.Blocking) != 1 || diff.Blocking[0].Kind != "column_conversion" || diff.Blocking[0].Rows != 1 {
			t.Fatalf("conversion findings = %#v", diff.Blocking)
		}
	})

	t.Run("not null", func(t *testing.T) {
		diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`)
		if err != nil {
			t.Fatal(err)
		}
		if len(diff.Blocking) != 1 || diff.Blocking[0].Kind != "not_null_existing_nulls" || diff.Blocking[0].Rows != 1 {
			t.Fatalf("not-null findings = %#v", diff.Blocking)
		}
	})
}

func TestSchemaDiffFindsUniquenessCollisionsCreatedByConversion(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	for id, value := range []string{"1", "01"} {
		if _, err := client.Create(ctx, "events", map[string]any{"id": id + 1, "value": value}); err != nil {
			t.Fatal(err)
		}
	}

	diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, unique: true }
`)
	if err != nil {
		t.Fatal(err)
	}
	if len(diff.Blocking) != 1 || diff.Blocking[0].Kind != "unique_index_duplicates" || diff.Blocking[0].Rows != 1 {
		t.Fatalf("post-conversion uniqueness findings = %#v", diff.Blocking)
	}
}

func TestSchemaDiffFindsUniquenessCollisionsFromAddedColumnMissingValues(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	for id := 1; id <= 2; id++ {
		if _, err := client.Create(ctx, "events", map[string]any{"id": id}); err != nil {
			t.Fatal(err)
		}
	}

	diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: code, type: string, default: same, unique: true }
`)
	if err != nil {
		t.Fatal(err)
	}
	if len(diff.Blocking) != 1 || diff.Blocking[0].Kind != "unique_index_duplicates" || diff.Blocking[0].Rows != 1 {
		t.Fatalf("historical-missing uniqueness findings = %#v", diff.Blocking)
	}
}

func TestSchemaDiffComposesOnlineTransitions(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true, index: true }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}

	t.Run("retained physical index is rebuilt after replacement", func(t *testing.T) {
		diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, index: true }
`)
		if err != nil {
			t.Fatal(err)
		}
		if len(diff.Blocking) != 0 {
			t.Fatalf("retained-index findings = %#v", diff.Blocking)
		}
		statements := schemaProgramStatements(t, diff)
		if len(statements) != 3 || statements[0].(map[string]any)["kind"] != "delete_index" ||
			statements[1].(map[string]any)["kind"] != "start_column_replacement" ||
			statements[2].(map[string]any)["kind"] != "start_index_build" {
			t.Fatalf("retained-index program = %#v", statements)
		}
		after, ok := statements[2].(map[string]any)["after"].([]any)
		if !ok || len(after) != 1 || after[0] != statements[1].(map[string]any)["name"] {
			t.Fatalf("retained-index dependency = %#v", statements[2])
		}
	})

	t.Run("replacement then new index has a durable completion dependency", func(t *testing.T) {
		diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, unique: true }
`)
		if err != nil {
			t.Fatal(err)
		}
		if len(diff.Blocking) != 0 {
			t.Fatalf("staging findings = %#v", diff.Blocking)
		}
		statements := schemaProgramStatements(t, diff)
		var replacementName string
		var build map[string]any
		for _, raw := range statements {
			statement := raw.(map[string]any)
			switch statement["kind"] {
			case "start_column_replacement":
				replacementName = statement["name"].(string)
			case "start_index_build":
				build = statement
			}
		}
		if replacementName == "" || build == nil {
			t.Fatalf("dependent transition program = %#v", statements)
		}
		after, ok := build["after"].([]any)
		if !ok || len(after) != 1 || after[0] != replacementName {
			t.Fatalf("index after = %#v, want [%q]", build["after"], replacementName)
		}
	})

	t.Run("deleting the index admits replacement", func(t *testing.T) {
		diff, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true }
`)
		if err != nil {
			t.Fatal(err)
		}
		if len(diff.Blocking) != 0 {
			t.Fatalf("delete-then-replace findings = %#v", diff.Blocking)
		}
		statements := schemaProgramStatements(t, diff)
		if len(statements) != 2 || statements[0].(map[string]any)["kind"] != "delete_index" ||
			statements[1].(map[string]any)["kind"] != "start_column_replacement" {
			t.Fatalf("delete-then-replace program = %#v", statements)
		}
	})
}

func TestSchemaDiffReportsValidConstraintBeforeNullabilityRelaxation(t *testing.T) {
	const nullable = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const required = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, nullable); err != nil {
		t.Fatal(err)
	}
	validation, err := migrateSchema(ctx, client, required)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.WaitSchemaMigration(ctx, validation); err != nil {
		t.Fatal(err)
	}

	diff, err := client.SchemaDiff(ctx, nullable)
	if err != nil {
		t.Fatal(err)
	}
	if len(diff.Blocking) != 1 || diff.Blocking[0].Kind != "column_replacement_dependency" ||
		!strings.Contains(diff.Blocking[0].Summary, "valid constraint") {
		t.Fatalf("nullability-relaxation findings = %#v", diff.Blocking)
	}
}

func TestSchemaMigrateOnlineReplacementIsIdempotentAndFencesWrites(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true }
`
	client := testServerInMode(t, model.ModeSchema)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	diff, err := client.SchemaDiff(ctx, desired)
	if err != nil {
		t.Fatal(err)
	}
	expected := protocol.SchemaIdentity{SchemaVersion: diff.CurrentVersion, SchemaHash: diff.CurrentHash}
	started, err := client.SchemaMigrate(ctx, desired, expected, false)
	if err != nil {
		t.Fatalf("start replacement migration: %v", err)
	}
	if started.State != protocol.SchemaMigrationConverging ||
		started.DesiredHash == "" || started.SchemaHash == started.DesiredHash ||
		len(started.TransitionIDs) != 1 {
		t.Fatalf("started migration = %#v", started)
	}
	transitions, err := client.SchemaTransitions(ctx)
	if err != nil || len(transitions) != 1 || transitions[0].TransitionKind != "column_replacement" {
		t.Fatalf("started transitions = %#v, %v", transitions, err)
	}
	originalID := transitions[0].TransitionID
	if started.TransitionIDs[0] != originalID {
		t.Fatalf("migration transition = %q, administrative transition = %q", started.TransitionIDs[0], originalID)
	}
	replayed, err := client.SchemaMigrate(ctx, desired, expected, false)
	if err != nil {
		t.Fatalf("lost-response replay: %v", err)
	}
	if replayed.State != protocol.SchemaMigrationConverging ||
		len(replayed.TransitionIDs) != 1 || replayed.TransitionIDs[0] != originalID {
		t.Fatalf("lost-response replay = %#v", replayed)
	}
	competing, err := client.SchemaDiff(ctx, `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: bool, nullable: true }
`)
	if err != nil {
		t.Fatal(err)
	}
	if len(competing.Blocking) != 1 || competing.Blocking[0].Kind != "active_schema_transition_conflict" {
		t.Fatalf("competing target findings = %#v", competing.Blocking)
	}

	if _, err := client.Create(ctx, "events", map[string]any{"id": 1, "value": "42"}); err != nil {
		t.Fatalf("convertible foreground write: %v", err)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": 2, "value": "not-an-integer"}); err == nil {
		t.Fatal("unconvertible foreground write succeeded during replacement")
	}

	recovered, err := migrateSchema(ctx, client, desired)
	if err != nil {
		t.Fatalf("idempotent re-apply: %v", err)
	}
	if recovered.State != protocol.SchemaMigrationConverging ||
		len(recovered.TransitionIDs) != 1 || recovered.TransitionIDs[0] != originalID {
		t.Fatalf("recovered migration = %#v", recovered)
	}
	transitions, err = client.SchemaTransitions(ctx)
	if err != nil || len(transitions) != 1 || transitions[0].TransitionID != originalID {
		t.Fatalf("re-apply transitions = %#v, %v; want only %q", transitions, err, originalID)
	}
}

func TestSchemaMigrateOnlineReplacementConvergesThroughPublicAPI(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": 1, "value": "42"}); err != nil {
		t.Fatal(err)
	}
	diff, err := client.SchemaDiff(ctx, desired)
	if err != nil {
		t.Fatal(err)
	}
	expected := protocol.SchemaIdentity{SchemaVersion: diff.CurrentVersion, SchemaHash: diff.CurrentHash}
	started, err := client.SchemaMigrate(ctx, desired, expected, false)
	if err != nil {
		t.Fatal(err)
	}
	ready, err := client.WaitSchemaMigration(ctx, started)
	if err != nil {
		t.Fatal(err)
	}
	if ready.State != protocol.SchemaMigrationReady || ready.SchemaHash != ready.DesiredHash {
		t.Fatalf("ready migration = %#v", ready)
	}
	replayed, err := client.SchemaMigrate(ctx, desired, expected, false)
	if err != nil || replayed.State != protocol.SchemaMigrationReady || replayed.SchemaHash != replayed.DesiredHash {
		t.Fatalf("completed lost-response replay = %#v, %v", replayed, err)
	}
	tables, err := client.Tables(ctx)
	if err != nil || len(tables) != 1 || len(tables[0].Columns) != 2 || tables[0].Columns[1].Type != "int64" {
		t.Fatalf("tables after replacement = %#v, %v", tables, err)
	}
	reapplied, err := migrateSchema(ctx, client, desired)
	if err != nil || reapplied.State != protocol.SchemaMigrationReady || len(reapplied.TransitionIDs) != 0 {
		t.Fatalf("ready re-apply = %#v, %v", reapplied, err)
	}
}

func TestSchemaMigrateChainsReplacementIntoIndexBuild(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, unique: true }
`
	client := testServer(t)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	for id, value := range []string{"42", "43"} {
		if _, err := client.Create(ctx, "events", map[string]any{"id": id + 1, "value": value}); err != nil {
			t.Fatal(err)
		}
	}

	started, err := migrateSchema(ctx, client, desired)
	if err != nil {
		t.Fatal(err)
	}
	if started.State != protocol.SchemaMigrationConverging || len(started.TransitionIDs) != 2 {
		t.Fatalf("started chained migration = %#v", started)
	}
	ready, err := client.WaitSchemaMigration(ctx, started)
	if err != nil {
		t.Fatal(err)
	}
	if ready.State != protocol.SchemaMigrationReady || ready.SchemaHash != ready.DesiredHash {
		t.Fatalf("ready chained migration = %#v", ready)
	}
	tables, err := client.Tables(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(tables) != 1 || len(tables[0].Columns) != 2 || tables[0].Columns[1].Type != "int64" ||
		len(tables[0].Indexes) != 1 || !tables[0].Indexes[0].Unique {
		t.Fatalf("schema after chained migration = %#v", tables)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": 3, "value": 42}); err == nil {
		t.Fatal("published unique index accepted a duplicate converted value")
	}
}

func TestSchemaMigrateDependentGraphRetryReusesBothTransitions(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, index: true }
`
	client := testServerInMode(t, model.ModeSchema)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	started, err := migrateSchema(ctx, client, desired)
	if err != nil {
		t.Fatal(err)
	}
	if len(started.TransitionIDs) != 2 {
		t.Fatalf("started graph = %#v", started)
	}
	retried, err := migrateSchema(ctx, client, desired)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(started.TransitionIDs, retried.TransitionIDs) {
		t.Fatalf("retry transition IDs = %v, want %v", retried.TransitionIDs, started.TransitionIDs)
	}

	transitions, err := client.SchemaTransitions(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if len(transitions) != 2 {
		t.Fatalf("durable graph = %#v", transitions)
	}
	var replacementID string
	var index radclient.TransitionControl
	for _, transition := range transitions {
		switch transition.TransitionKind {
		case radclient.TransitionColumnReplacement:
			replacementID = transition.TransitionID
		case radclient.TransitionIndexBuild:
			index = transition
		}
	}
	if replacementID == "" || index.TransitionID == "" || index.State != radclient.TransitionWaiting ||
		!slices.Equal(index.Prerequisites, []string{replacementID}) {
		t.Fatalf("durable dependent graph = %#v", transitions)
	}
}

func TestSchemaMigrateConcurrentWritesCrossDependentPublication(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true, unique: true }
`
	client := testServer(t)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	for id := 1; id <= 32; id++ {
		if _, err := client.Create(ctx, "events", map[string]any{"id": id, "value": fmt.Sprint(id)}); err != nil {
			t.Fatal(err)
		}
	}

	started, err := migrateSchema(ctx, client, desired)
	if err != nil {
		t.Fatal(err)
	}
	if len(started.TransitionIDs) != 2 {
		t.Fatalf("started migration = %#v", started)
	}

	const writers = 8
	const rowsPerWriter = 12
	errors := make(chan error, writers)
	var wait sync.WaitGroup
	for writer := range writers {
		wait.Add(1)
		go func() {
			defer wait.Done()
			for offset := range rowsPerWriter {
				id := 33 + writer*rowsPerWriter + offset
				for {
					_, sourceErr := client.Create(ctx, "events", map[string]any{"id": id, "value": fmt.Sprint(id)})
					if sourceErr == nil {
						break
					}
					_, targetErr := client.Create(ctx, "events", map[string]any{"id": id, "value": id})
					if targetErr == nil {
						break
					}
					_, found, getErr := client.Get(ctx, "events", map[string]any{"id": id})
					if getErr == nil && found {
						break
					}
					select {
					case <-ctx.Done():
						errors <- fmt.Errorf("writer %d row %d: source=%v target=%v get=%v: %w", writer, id, sourceErr, targetErr, getErr, ctx.Err())
						return
					case <-time.After(2 * time.Millisecond):
					}
				}
			}
		}()
	}
	wait.Wait()
	close(errors)
	for err := range errors {
		t.Error(err)
	}
	if t.Failed() {
		return
	}

	ready, err := client.WaitSchemaMigration(ctx, started)
	if err != nil {
		t.Fatal(err)
	}
	if ready.State != protocol.SchemaMigrationReady {
		t.Fatalf("migration = %#v", ready)
	}
	rows, err := client.Query(ctx, scanOrdered("events", "e", "id"))
	if err != nil {
		t.Fatal(err)
	}
	wantRows := 32 + writers*rowsPerWriter
	if len(rows) != wantRows {
		t.Fatalf("rows after concurrent migration = %d, want %d", len(rows), wantRows)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": wantRows + 1, "value": 1}); err == nil {
		t.Fatal("published unique index accepted a concurrent row's duplicate value")
	}
}

func TestSchemaMigrateCancelledNotNullValidationCanBeReapplied(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`
	client := testServerInMode(t, model.ModeSchema)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	started, err := migrateSchema(ctx, client, desired)
	if err != nil || len(started.TransitionIDs) != 1 {
		t.Fatalf("start validation = %#v, %v", started, err)
	}
	recovered, err := migrateSchema(ctx, client, desired)
	if err != nil || len(recovered.TransitionIDs) != 1 ||
		recovered.TransitionIDs[0] != started.TransitionIDs[0] {
		t.Fatalf("recover active validation = %#v, %v", recovered, err)
	}
	if _, err := client.CancelSchemaTransition(ctx, started.TransitionIDs[0]); err != nil {
		t.Fatal(err)
	}
	retried, err := migrateSchema(ctx, client, desired)
	if err != nil {
		t.Fatalf("reapply cancelled validation: %v", err)
	}
	if len(retried.TransitionIDs) != 1 || retried.TransitionIDs[0] == started.TransitionIDs[0] {
		t.Fatalf("retried validation = %#v", retried)
	}
}

func TestSchemaMigrateOnlineIndexIsIdempotentAndCapturesWrites(t *testing.T) {
	const initial = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`
	const desired = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
    indexes:
      - { name: events_value_idx, columns: [value] }
`
	client := testServerInMode(t, model.ModeSchema)
	ctx := context.Background()
	if _, err := migrateSchema(ctx, client, initial); err != nil {
		t.Fatal(err)
	}
	started, err := migrateSchema(ctx, client, desired)
	if err != nil || started.State != protocol.SchemaMigrationConverging || len(started.TransitionIDs) != 1 {
		t.Fatalf("start index migration = %#v, %v", started, err)
	}
	if _, err := client.Create(ctx, "events", map[string]any{"id": 1, "value": "during-build"}); err != nil {
		t.Fatalf("foreground indexed write: %v", err)
	}
	recovered, err := migrateSchema(ctx, client, desired)
	if err != nil || len(recovered.TransitionIDs) != 1 ||
		recovered.TransitionIDs[0] != started.TransitionIDs[0] {
		t.Fatalf("recover index migration = %#v, %v", recovered, err)
	}
	transitions, err := client.SchemaTransitions(ctx)
	if err != nil || len(transitions) != 1 || transitions[0].TransitionKind != "index_build" {
		t.Fatalf("index transitions = %#v, %v", transitions, err)
	}
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
