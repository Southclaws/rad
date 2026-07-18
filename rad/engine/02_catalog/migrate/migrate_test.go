// These tests document the migration differ: which schema edits produce
// which steps, how stable IDs distinguish renames from replacements, how new
// tables are dependency-ordered, and which transformations are refused.
package migrate_test

import (
	"context"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

// currentFrom materializes a schema source into a real catalog and returns
// its tables — the differ always runs against actual catalog state.
func currentFrom(t *testing.T, src string) []catalog.Table {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)
	s := parse(t, src)
	for _, tb := range s.Tables {
		if _, err := cat.CreateTable(ctx, tb.Def); err != nil {
			t.Fatal(err)
		}
	}
	tables, err := cat.ListTables(ctx)
	if err != nil {
		t.Fatal(err)
	}
	return tables
}

func parse(t *testing.T, src string) *schema.Schema {
	t.Helper()
	s, err := schema.Parse("test.rad", []byte(src))
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func diff(t *testing.T, current []catalog.Table, desiredSrc string) []migrate.Step {
	t.Helper()
	steps, err := migrate.Diff(current, parse(t, desiredSrc))
	if err != nil {
		t.Fatal(err)
	}
	return steps
}

func stepStrings(steps []migrate.Step) []string {
	out := make([]string, len(steps))
	for i, s := range steps {
		out[i] = s.String()
	}
	return out
}

const v1 = `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: name, type: string, index: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
`

// An empty database diffs to a create for every table, dependency-ordered:
// users (referenced) must precede boards (referencing).
func TestInitialMigration(t *testing.T) {
	steps := diff(t, nil, v1)
	got := stepStrings(steps)
	want := []string{"create table users", "create table boards"}
	if strings.Join(got, "; ") != strings.Join(want, "; ") {
		t.Fatalf("got %v, want %v", got, want)
	}
}

// The same schema diffs to nothing — migrations are idempotent.
func TestNoChanges(t *testing.T) {
	steps := diff(t, currentFrom(t, v1), v1)
	if len(steps) != 0 {
		t.Fatalf("expected empty plan, got %v", stepStrings(steps))
	}
}

func TestCreateColumnAndIndex(t *testing.T) {
	steps := diff(t, currentFrom(t, v1), `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,    type: string, pk: true }
      - { id: 2, name: name,  type: string, index: true }
      - { id: 3, name: email, type: string, nullable: true, unique: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id, index: true }
`)
	got := stepStrings(steps)
	want := []string{
		"create column users.email",
		"create index boards_user_id_idx on boards",
		"create index users_email_uq on users",
	}
	// Order within groups is deterministic; adds come before index adds.
	if len(got) != 3 || got[0] != want[0] {
		t.Fatalf("got %v", got)
	}
}

// A stable column ID makes a rename deterministic. The index over the renamed
// column is structurally unchanged, so it does not need a rebuild.
func TestColumnRenameByID(t *testing.T) {
	current := currentFrom(t, v1)

	steps := diff(t, current, `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,        type: string, pk: true }
      - { id: 2, name: full_name, type: string, index: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
`)
	got := stepStrings(steps)
	if len(got) != 1 || got[0] != "rename column users.name -> full_name" {
		t.Fatalf("rename: %v", got)
	}
}

func TestColumnIdentityReplacementRejected(t *testing.T) {
	_, err := migrate.Diff(currentFrom(t, v1), parse(t, `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,        type: string, pk: true }
      - { id: 3, name: name, type: string, nullable: true, index: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
`))
	if err == nil || !strings.Contains(err.Error(), "changes schema ID 2 -> 3") {
		t.Fatalf("got %v", err)
	}
}

func TestTableRenameByID(t *testing.T) {
	steps := diff(t, currentFrom(t, v1), `
tables:
  - id: 1
    name: people
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: name, type: string, index: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: people.id }
`)
	got := stepStrings(steps)
	if len(got) != 1 || got[0] != "rename table users -> people" {
		t.Fatalf("got %v", got)
	}
}

func TestReferencedPrimaryKeyRenameByID(t *testing.T) {
	current := currentFrom(t, `
tables:
  - id: 1
    name: parents
    columns:
      - { id: 1, name: id, type: string, pk: true }
  - id: 2
    name: children
    columns:
      - { id: 1, name: id,        type: string, pk: true }
      - { id: 2, name: parent_id, type: string, ref: parents.id }
`)
	steps := diff(t, current, `
tables:
  - id: 1
    name: parents
    columns:
      - { id: 1, name: parent_key, type: string, pk: true }
  - id: 2
    name: children
    columns:
      - { id: 1, name: id,        type: string, pk: true }
      - { id: 2, name: parent_id, type: string, ref: parents.parent_key }
`)
	got := stepStrings(steps)
	if len(got) != 1 || got[0] != "rename column parents.id -> parent_key" {
		t.Fatalf("got %v", got)
	}
}

func TestDeleteTable(t *testing.T) {
	steps := diff(t, currentFrom(t, v1), `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: name, type: string, index: true }
`)
	got := stepStrings(steps)
	if len(got) != 1 || got[0] != "delete table boards" {
		t.Fatalf("got %v", got)
	}
}

// Deleting a table that survivors still reference is refused.
func TestDeleteReferencedTableRejected(t *testing.T) {
	_, err := migrate.Diff(currentFrom(t, v1), parse(t, `
tables:
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
`))
	if err == nil || !strings.Contains(err.Error(), "references deleted table") {
		t.Fatalf("got %v", err)
	}
}

func TestUnsupportedChangesRejected(t *testing.T) {
	current := currentFrom(t, v1)
	boards := `
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
`
	cases := []struct {
		name, desired, wantErr string
	}{
		{"type change", `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: name, type: int64, index: true }
` + boards, "changing type"},
		{"nullability change", `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: name, type: string, nullable: true, index: true }
` + boards, "changing nullability"},
		{"pk change", `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string }
      - { id: 2, name: name, type: string, pk: true, index: true }
` + boards, "primary key"},
		{"fk change", `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: name, type: string, index: true }
  - id: 2
    name: boards
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string }
`, "foreign keys"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := migrate.Diff(current, parse(t, tc.desired))
			if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
				t.Fatalf("got %v, want %q", err, tc.wantErr)
			}
		})
	}
}

// New tables are created in FK dependency order even when declared
// backwards; circular dependencies are detected.
func TestCreateOrdering(t *testing.T) {
	steps := diff(t, nil, `
tables:
  - id: 1
    name: task_labels
    columns:
      - { id: 1, name: task_id,  type: string, ref: tasks.id }
      - { id: 2, name: label_id, type: string, ref: labels.id }
    primary_key: [task_id, label_id]
  - id: 2
    name: tasks
    columns:
      - { id: 1, name: id,        type: string, pk: true }
      - { id: 2, name: parent_id, type: string, nullable: true, ref: tasks.id }
  - id: 3
    name: labels
    columns:
      - { id: 1, name: id, type: string, pk: true }
`)
	got := stepStrings(steps)
	want := []string{"create table labels", "create table tasks", "create table task_labels"}
	if strings.Join(got, "; ") != strings.Join(want, "; ") {
		t.Fatalf("got %v, want %v", got, want)
	}

	_, err := migrate.Diff(nil, parse(t, `
tables:
  - id: 1
    name: a
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: b_id, type: string, ref: b.id }
  - id: 2
    name: b
    columns:
      - { id: 1, name: id,   type: string, pk: true }
      - { id: 2, name: a_id, type: string, ref: a.id }
`))
	if err == nil || !strings.Contains(err.Error(), "circular") {
		t.Fatalf("got %v", err)
	}
}

// Deleting a parent and its referencing child together must delete the child
// first — the catalog refuses to delete a table that is still referenced. The
// names are chosen adversarially: the parent sorts before the child, so a
// name-ordered plan would delete the parent first and fail at apply time.
func TestDeletesOrderedReferencingTableFirst(t *testing.T) {
	current := currentFrom(t, `
tables:
  - id: 1
    name: accounts
    columns:
      - { id: 1, name: id, type: string, pk: true }
  - id: 2
    name: zposts
    columns:
      - { id: 1, name: id,         type: string, pk: true }
      - { id: 2, name: account_id, type: string, ref: accounts.id }
`)

	got := stepStrings(diff(t, current, `
tables:
  - id: 3
    name: unrelated
    columns:
      - { id: 1, name: id, type: string, pk: true }
`))

	want := []string{
		"create table unrelated",
		"delete table zposts",
		"delete table accounts",
	}
	if len(got) != len(want) {
		t.Fatalf("steps = %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("step %d = %q, want %q (full plan %v)", i, got[i], want[i], got)
		}
	}
}
