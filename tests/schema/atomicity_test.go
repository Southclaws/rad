package schema_test

import (
	"reflect"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
)

func TestBlockingPreflightAndRejectedApplyAreObservational(t *testing.T) {
	tests := []struct {
		name        string
		initial     string
		rows        []map[string]any
		desired     string
		finding     string
		findingRows uint64
	}{
		{
			name: "strict conversion",
			initial: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`,
			rows: []map[string]any{{"id": 1, "value": "not-an-integer"}},
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true }
`,
			finding: "column_conversion", findingRows: 1,
		},
		{
			name: "existing null",
			initial: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`,
			rows: []map[string]any{{"id": 1, "value": nil}},
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`,
			finding: "not_null_existing_nulls", findingRows: 1,
		},
		{
			name: "uniqueness introduced by conversion",
			initial: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string }
`,
			rows: []map[string]any{{"id": 1, "value": "1"}, {"id": 2, "value": "01"}},
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, unique: true }
`,
			finding: "unique_index_duplicates", findingRows: 1,
		},
		{
			name: "historical missing value collision",
			initial: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
`,
			rows: []map[string]any{{"id": 1}, {"id": 2}},
			desired: `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: code, type: string, default: same, unique: true }
`,
			finding: "unique_index_duplicates", findingRows: 1,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			db := newDatabase(t)
			db.SchemaMigrateReady(test.initial, false)
			for _, row := range test.rows {
				if _, err := db.Client.Create(t.Context(), "events", row); err != nil {
					t.Fatal(err)
				}
			}
			beforeSchema, err := db.Client.Schema(t.Context())
			if err != nil {
				t.Fatal(err)
			}
			beforeTables, err := db.Client.Tables(t.Context())
			if err != nil {
				t.Fatal(err)
			}

			diff := db.SchemaPlan(test.desired)
			if len(diff.Blocking) != 1 || diff.Blocking[0].Kind != test.finding || diff.Blocking[0].Rows != test.findingRows {
				t.Fatalf("blocking findings = %#v, want %q affecting %d row(s)", diff.Blocking, test.finding, test.findingRows)
			}
			if _, err := db.SchemaApply(test.desired, diff, false); err == nil {
				t.Fatal("blocked migration applied")
			} else {
				requireProblemCode(t, err, protocol.CodeInvalid)
			}

			afterSchema, err := db.Client.Schema(t.Context())
			if err != nil {
				t.Fatal(err)
			}
			afterTables, err := db.Client.Tables(t.Context())
			if err != nil {
				t.Fatal(err)
			}
			if !reflect.DeepEqual(afterSchema, beforeSchema) || !reflect.DeepEqual(afterTables, beforeTables) {
				t.Fatalf("rejected migration changed catalog:\nbefore=%#v %#v\nafter=%#v %#v", beforeSchema, beforeTables, afterSchema, afterTables)
			}
			transitions, err := db.Client.SchemaTransitions(t.Context())
			if err != nil {
				t.Fatal(err)
			}
			if len(transitions) != 0 {
				t.Fatalf("rejected migration created durable work: %#v", transitions)
			}
			for _, row := range test.rows {
				if _, found, err := db.Client.Get(t.Context(), "events", map[string]any{"id": row["id"]}); err != nil || !found {
					t.Fatalf("source row %v lost after rejected migration: found=%v err=%v", row["id"], found, err)
				}
			}
		})
	}
}
