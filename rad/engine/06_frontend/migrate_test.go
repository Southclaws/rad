package frontend_test

// The migration workflow end to end at the frontend: evolve a live database
// from schema v1 to v2 (rename + add column + new index + new table) with
// data in place, and verify both the data and the new index survived.

import (
	"context"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	radschema "github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/engine/06_frontend/resultjson"
)

const trackerV1 = `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true, default: uuid() }
      - { id: 2, name: name, type: string }
  - id: 2
    name: tasks
    columns:
      - { id: 1, name: id,      type: string, pk: true, default: uuid() }
      - { id: 2, name: user_id, type: string, ref: users.id }
      - { id: 3, name: title,   type: string }
      - { id: 4, name: done,    type: bool, default: false }
`

const trackerV2 = `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,       type: string, pk: true, default: uuid() }
      - { id: 2, name: username, type: string }
      - { id: 3, name: email,    type: string, nullable: true }
  - id: 2
    name: tasks
    columns:
      - { id: 1, name: id,      type: string, pk: true, default: uuid() }
      - { id: 2, name: user_id, type: string, ref: users.id }
      - { id: 3, name: title,   type: string }
      - { id: 4, name: done,    type: bool, default: false }
    indexes:
      - { columns: [user_id, done] }
  - id: 3
    name: comments
    columns:
      - { id: 1, name: id,      type: string, pk: true, default: uuid() }
      - { id: 2, name: task_id, type: string, ref: tasks.id, index: true }
      - { id: 3, name: body,    type: string }
`

func migrateTo(t *testing.T, db *frontend.DB, ctx context.Context, src string) []string {
	t.Helper()
	steps, err := db.MigrateFile(ctx, "rad.schema.yaml", []byte(src))
	if err != nil {
		t.Fatal(err)
	}
	out := make([]string, len(steps))
	for i, s := range steps {
		out[i] = s.String()
	}
	return out
}

func TestMigrationWorkflow(t *testing.T) {
	ctx := context.Background()
	db := frontend.Open(memStore(t))
	if _, err := db.Catalog().InitMode(ctx, model.ModeSchema); err != nil {
		t.Fatal(err)
	}

	// v1 on an empty store creates everything (including the schema).
	steps := migrateTo(t, db, ctx, trackerV1)
	if len(steps) != 2 {
		t.Fatalf("v1 steps: %v", steps)
	}
	if revision, err := db.Catalog().Revision(ctx); err != nil || revision.Version != 1 {
		t.Fatalf("v1 revision = %+v, %v; want one revision for two steps", revision, err)
	}
	assertRevisionSchema(t, db, ctx, trackerV1)

	// Live data under v1.
	if err := db.Insert(ctx, "users", lir.Row{"id": lir.Text("u1"), "name": lir.Text("ada")}); err != nil {
		t.Fatal(err)
	}
	if err := db.Insert(ctx, "tasks", lir.Row{"id": lir.Text("t1"), "user_id": lir.Text("u1"), "title": lir.Text("ship v0")}); err != nil {
		t.Fatal(err)
	}

	// v1 -> v1 is a no-op.
	if steps := migrateTo(t, db, ctx, trackerV1); len(steps) != 0 {
		t.Fatalf("re-migrate should be empty, got %v", steps)
	}
	if revision, _ := db.Catalog().Revision(ctx); revision.Version != 1 {
		t.Fatalf("no-op migration moved schema version to %d", revision.Version)
	}

	// v1 -> v2: rename users.name, add users.email, add composite index
	// (backfilled), create comments.
	steps = migrateTo(t, db, ctx, trackerV2)
	if len(steps) != 4 {
		t.Fatalf("v2 steps: %v", steps)
	}
	if revision, err := db.Catalog().Revision(ctx); err != nil || revision.Version != 3 {
		t.Fatalf("v2 revision = %+v, %v; want start revision plus online publication", revision, err)
	}
	assertRevisionSchema(t, db, ctx, trackerV2)

	// Existing data is intact under the new names.
	user, ok, err := db.Get(ctx, "users", lir.Row{"id": lir.Text("u1")})
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !user["username"].Equal(lir.Text("ada")) {
		t.Fatalf("rename lost data: %v", user)
	}
	if !user["email"].Null {
		t.Fatalf("added column should read NULL: %v", user)
	}

	// The new index was backfilled: a query pinned to its leading column
	// rides it, and access-path narrowing means a missing entry would lose
	// the row.
	d, err := db.Execute(ctx, lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: lir.Filter{
			Input: lir.Scan{Table: "tasks", Scope: "t"},
			Pred: lir.Binary{
				Op: lir.OpEq,
				L:  lir.Column{Scope: "t", Name: "user_id"},
				R:  lir.Literal{Raw: "u1"},
			},
		},
		Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "t", Name: "id"}}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	rows, _ := resultjson.Datum(d).([]any)
	if len(rows) != 1 || rows[0].(map[string]any)["title"] != "ship v0" {
		t.Fatalf("backfilled index scan: %v", resultjson.Datum(d))
	}

	// The new table works.
	if err := db.Insert(ctx, "comments", lir.Row{"id": lir.Text("c1"), "task_id": lir.Text("t1"), "body": lir.Text("nice")}); err != nil {
		t.Fatal(err)
	}

	// v2 -> v2 is a no-op again.
	if steps := migrateTo(t, db, ctx, trackerV2); len(steps) != 0 {
		t.Fatalf("re-migrate v2 should be empty, got %v", steps)
	}
	history, err := db.Catalog().Revisions(ctx)
	if err != nil || len(history) != 3 {
		t.Fatalf("migration history = %+v, %v", history, err)
	}
}

func assertRevisionSchema(t *testing.T, db *frontend.DB, ctx context.Context, source string) {
	t.Helper()
	desired, err := radschema.Parse("rad.schema.yaml", []byte(source))
	if err != nil {
		t.Fatal(err)
	}
	revision, err := db.Catalog().Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	equal, err := revision.Schema.Equal(desired.Canonical())
	if err != nil || !equal {
		t.Fatalf("revision schema differs from desired schema: equal=%v err=%v\nrevision=%+v\ndesired=%+v",
			equal, err, revision.Schema, desired.Canonical())
	}
	if err := db.Catalog().ValidateCurrentSchema(ctx); err != nil {
		t.Fatalf("revision schema differs from physical catalog: %v", err)
	}
}

func TestMigrationChangesInsertDefaultWithoutRewritingHistory(t *testing.T) {
	ctx := context.Background()
	db := frontend.Open(memStore(t))
	if _, err := db.Catalog().InitMode(ctx, model.ModeSchema); err != nil {
		t.Fatal(err)
	}
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
	migrateTo(t, db, ctx, initial)
	if err := db.Insert(ctx, "items", lir.Row{"id": lir.Int64(1)}); err != nil {
		t.Fatal(err)
	}
	steps := migrateTo(t, db, ctx, updated)
	if len(steps) != 1 || steps[0] != "change column default items.status" {
		t.Fatalf("default migration steps = %v", steps)
	}
	if err := db.Insert(ctx, "items", lir.Row{"id": lir.Int64(2)}); err != nil {
		t.Fatal(err)
	}
	old, ok, err := db.Get(ctx, "items", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok || !old["status"].Equal(lir.Text("active")) {
		t.Fatalf("old row = %v found=%v err=%v", old, ok, err)
	}
	current, ok, err := db.Get(ctx, "items", lir.Row{"id": lir.Int64(2)})
	if err != nil || !ok || !current["status"].Equal(lir.Text("pending")) {
		t.Fatalf("new row = %v found=%v err=%v", current, ok, err)
	}
	assertRevisionSchema(t, db, ctx, updated)
}

// A unique-index backfill over duplicate data is refused — and the refusal
// rolls back the index registration too, so the catalog never exposes an
// index missing its entries. Fixing the data and re-migrating then succeeds
// and the constraint is live.
func TestUniqueBackfillRejectsDuplicates(t *testing.T) {
	ctx := context.Background()
	db := frontend.Open(memStore(t))
	if _, err := db.Catalog().InitMode(ctx, model.ModeSchema); err != nil {
		t.Fatal(err)
	}
	migrateTo(t, db, ctx, trackerV1)

	for _, id := range []string{"u1", "u2"} {
		if err := db.Insert(ctx, "users", lir.Row{"id": lir.Text(id), "name": lir.Text("dup")}); err != nil {
			t.Fatal(err)
		}
	}

	uniqueName := `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id,   type: string, pk: true, default: uuid() }
      - { id: 2, name: name, type: string, unique: true }
      - { id: 3, name: note, type: string, nullable: true }
  - id: 2
    name: tasks
    columns:
      - { id: 1, name: id,      type: string, pk: true, default: uuid() }
      - { id: 2, name: user_id, type: string, ref: users.id }
      - { id: 3, name: title,   type: string }
      - { id: 4, name: done,    type: bool, default: false }
`
	plan, err := db.PlanMigrationFile(ctx, "rad.schema.yaml", []byte(uniqueName))
	if err != nil {
		t.Fatal(err)
	}
	if len(plan.Blocking) != 1 || plan.Blocking[0].Kind != "unique_index_duplicates" {
		t.Fatalf("blocking findings = %+v", plan.Blocking)
	}
	if _, err := db.ApplyMigrationFile(ctx, "rad.schema.yaml", []byte(uniqueName), true); err == nil {
		t.Fatal("data-loss acceptance bypassed a blocking constraint")
	}
	_, err = db.MigrateFile(ctx, "rad.schema.yaml", []byte(uniqueName))
	if err == nil {
		t.Fatal("unique backfill over duplicates succeeded")
	}
	if revision, revisionErr := db.Catalog().Revision(ctx); revisionErr != nil || revision.Version != 1 {
		t.Fatalf("failed migration revision = %+v, %v; want unchanged v1", revision, revisionErr)
	}
	assertRevisionSchema(t, db, ctx, trackerV1)

	// The registration must have rolled back with the backfill: no index on
	// users. A registered-but-empty index would let reads silently drop rows.
	tables, err := db.Tables(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, tbl := range tables {
		if tbl.Name == "users" {
			if len(tbl.Indexes) != 0 {
				t.Fatalf("failed backfill left index registered on users: %+v", tbl.Indexes)
			}
			if _, ok := tbl.Column("note"); ok {
				t.Fatal("failed migration left an earlier column step committed")
			}
		}
	}

	// Fix the data; the same migration now applies and the constraint is live.
	if _, _, err := db.Update(ctx, "users", lir.Row{"id": lir.Text("u2")}, lir.Row{"name": lir.Text("unique")}); err != nil {
		t.Fatal(err)
	}
	if steps := migrateTo(t, db, ctx, uniqueName); len(steps) == 0 {
		t.Fatal("re-migration after fixing data applied no steps")
	}
	if revision, revisionErr := db.Catalog().Revision(ctx); revisionErr != nil || revision.Version < 3 {
		t.Fatalf("successful retry revision = %+v, %v; want start and online finalization revisions", revision, revisionErr)
	}
	if err := db.Insert(ctx, "users", lir.Row{"id": lir.Text("u3"), "name": lir.Text("dup")}); err == nil {
		t.Fatal("unique index registered by re-migration is not enforcing")
	}
}

func TestDestructiveMigrationRequiresExplicitAcceptance(t *testing.T) {
	ctx := context.Background()
	db := frontend.Open(memStore(t))
	if _, err := db.Catalog().InitMode(ctx, model.ModeSchema); err != nil {
		t.Fatal(err)
	}
	migrateTo(t, db, ctx, trackerV1)
	if err := db.Insert(ctx, "users", lir.Row{"id": lir.Text("u1"), "name": lir.Text("Ada")}); err != nil {
		t.Fatal(err)
	}

	withoutName := `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true, default: uuid() }
  - id: 2
    name: tasks
    columns:
      - { id: 1, name: id,      type: string, pk: true, default: uuid() }
      - { id: 2, name: user_id, type: string, ref: users.id }
      - { id: 3, name: title,   type: string }
      - { id: 4, name: done,    type: bool, default: false }
`
	plan, err := db.PlanMigrationFile(ctx, "rad.schema.yaml", []byte(withoutName))
	if err != nil {
		t.Fatal(err)
	}
	if len(plan.Destructive) != 1 || plan.Destructive[0].Rows != 1 {
		t.Fatalf("destructive findings = %+v", plan.Destructive)
	}
	if _, err := db.ApplyMigrationFile(ctx, "rad.schema.yaml", []byte(withoutName), false); err == nil {
		t.Fatal("destructive migration applied without consent")
	}
	if revision, _ := db.Catalog().Revision(ctx); revision.Version != 1 {
		t.Fatalf("rejected destructive migration moved version to %d", revision.Version)
	}
	if _, err := db.ApplyMigrationFile(ctx, "rad.schema.yaml", []byte(withoutName), true); err != nil {
		t.Fatal(err)
	}
	if revision, _ := db.Catalog().Revision(ctx); revision.Version != 2 {
		t.Fatalf("accepted destructive migration version = %d", revision.Version)
	}
}
