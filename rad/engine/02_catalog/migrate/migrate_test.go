// These tests document the migration differ: which schema edits produce
// which steps, how renamed_from hints turn delete+create into renames, how new
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
  - name: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string, index: true }
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: users.id }
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
  - name: users
    columns:
      - { name: id,    type: string, pk: true }
      - { name: name,  type: string, index: true }
      - { name: email, type: string, nullable: true, unique: true }
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: users.id, index: true }
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

// A rename hint converts what would be delete+create into a rename; without the
// hint the same edit is a delete plus a create.
func TestColumnRenameHint(t *testing.T) {
	current := currentFrom(t, v1)

	hinted := diff(t, current, `
tables:
  - name: users
    columns:
      - { name: id,        type: string, pk: true }
      - { name: full_name, type: string, index: true, renamed_from: name }
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: users.id }
`)
	got := stepStrings(hinted)
	// One step only: the index over the renamed column is structurally
	// unchanged (indexes are compared by shape, not name), so no delete or
	// backfill happens.
	if len(got) != 1 || got[0] != "rename column users.name -> full_name" {
		t.Fatalf("hinted rename: %v", got)
	}

	unhinted := diff(t, current, `
tables:
  - name: users
    columns:
      - { name: id,        type: string, pk: true }
      - { name: full_name, type: string, nullable: true, index: true }
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: users.id }
`)
	gotU := stepStrings(unhinted)
	joined := strings.Join(gotU, "; ")
	if !strings.Contains(joined, "create column users.full_name") || !strings.Contains(joined, "delete column users.name") {
		t.Fatalf("unhinted rename should be create+delete: %v", gotU)
	}
}

func TestTableRenameHint(t *testing.T) {
	steps := diff(t, currentFrom(t, v1), `
tables:
  - name: people
    renamed_from: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string, index: true }
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: people.id }
`)
	got := stepStrings(steps)
	if len(got) != 1 || got[0] != "rename table users -> people" {
		t.Fatalf("got %v", got)
	}
}

func TestDeleteTable(t *testing.T) {
	steps := diff(t, currentFrom(t, v1), `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string, index: true }
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
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: users.id }
`))
	if err == nil || !strings.Contains(err.Error(), "references deleted table") {
		t.Fatalf("got %v", err)
	}
}

func TestUnsupportedChangesRejected(t *testing.T) {
	current := currentFrom(t, v1)
	boards := `
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string, ref: users.id }
`
	cases := []struct {
		name, desired, wantErr string
	}{
		{"type change", `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: int64, index: true }
` + boards, "changing type"},
		{"nullability change", `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string, nullable: true, index: true }
` + boards, "changing nullability"},
		{"pk change", `
tables:
  - name: users
    columns:
      - { name: id,   type: string }
      - { name: name, type: string, pk: true, index: true }
` + boards, "primary key"},
		{"fk change", `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string, index: true }
  - name: boards
    columns:
      - { name: id,      type: string, pk: true }
      - { name: user_id, type: string }
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
  - name: task_labels
    columns:
      - { name: task_id,  type: string, ref: tasks.id }
      - { name: label_id, type: string, ref: labels.id }
    primary_key: [task_id, label_id]
  - name: tasks
    columns:
      - { name: id,        type: string, pk: true }
      - { name: parent_id, type: string, nullable: true, ref: tasks.id }
  - name: labels
    columns:
      - { name: id, type: string, pk: true }
`)
	got := stepStrings(steps)
	want := []string{"create table labels", "create table tasks", "create table task_labels"}
	if strings.Join(got, "; ") != strings.Join(want, "; ") {
		t.Fatalf("got %v, want %v", got, want)
	}

	_, err := migrate.Diff(nil, parse(t, `
tables:
  - name: a
    columns:
      - { name: id,   type: string, pk: true }
      - { name: b_id, type: string, ref: b.id }
  - name: b
    columns:
      - { name: id,   type: string, pk: true }
      - { name: a_id, type: string, ref: a.id }
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
  - name: accounts
    columns:
      - { name: id, type: string, pk: true }
  - name: zposts
    columns:
      - { name: id,         type: string, pk: true }
      - { name: account_id, type: string, ref: accounts.id }
`)

	got := stepStrings(diff(t, current, `
tables:
  - name: unrelated
    columns:
      - { name: id, type: string, pk: true }
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
