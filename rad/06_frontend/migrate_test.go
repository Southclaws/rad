package frontend_test

// The migration workflow end to end at the frontend: evolve a live database
// from schema v1 to v2 (rename + add column + new index + new table) with
// data in place, and verify both the data and the new index survived.

import (
	"context"
	"testing"

	lir "rad/rad/03_lir"
	frontend "rad/rad/06_frontend"
)

const trackerV1 = `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true, default: uuid() }
      - { name: name, type: string }
  - name: tasks
    columns:
      - { name: id,      type: string, pk: true, default: uuid() }
      - { name: user_id, type: string, ref: users.id }
      - { name: title,   type: string }
      - { name: done,    type: bool, default: false }
`

const trackerV2 = `
tables:
  - name: users
    columns:
      - { name: id,       type: string, pk: true, default: uuid() }
      - { name: username, type: string, renamed_from: name }
      - { name: email,    type: string, nullable: true }
  - name: tasks
    columns:
      - { name: id,      type: string, pk: true, default: uuid() }
      - { name: user_id, type: string, ref: users.id }
      - { name: title,   type: string }
      - { name: done,    type: bool, default: false }
    indexes:
      - { columns: [user_id, done] }
  - name: comments
    columns:
      - { name: id,      type: string, pk: true, default: uuid() }
      - { name: task_id, type: string, ref: tasks.id, index: true }
      - { name: body,    type: string }
`

func migrateTo(t *testing.T, db *frontend.DB, ctx context.Context, src string) []string {
	t.Helper()
	steps, err := db.MigrateFile(ctx, "schema.rad", []byte(src))
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

	// v1 on an empty store creates everything (including the schema).
	steps := migrateTo(t, db, ctx, trackerV1)
	if len(steps) != 2 {
		t.Fatalf("v1 steps: %v", steps)
	}

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

	// v1 -> v2: rename users.name, add users.email, add composite index
	// (backfilled), create comments.
	steps = migrateTo(t, db, ctx, trackerV2)
	if len(steps) != 4 {
		t.Fatalf("v2 steps: %v", steps)
	}

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

	// The new index was backfilled: the pre-existing task is findable
	// through it.
	rows, err := db.Query(ctx, lir.Query{Root: lir.IndexScan{
		Table: "tasks", Alias: "t", Index: "tasks_user_id_done_idx",
		Prefix: lir.Row{"user_id": lir.Text("u1")},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || !rows[0]["t.title"].Equal(lir.Text("ship v0")) {
		t.Fatalf("backfilled index scan: %v", rows)
	}

	// The new table works.
	if err := db.Insert(ctx, "comments", lir.Row{"id": lir.Text("c1"), "task_id": lir.Text("t1"), "body": lir.Text("nice")}); err != nil {
		t.Fatal(err)
	}

	// v2 -> v2 is a no-op again.
	if steps := migrateTo(t, db, ctx, trackerV2); len(steps) != 0 {
		t.Fatalf("re-migrate v2 should be empty, got %v", steps)
	}
}

// A unique-index backfill over duplicate data is refused — and the refusal
// rolls back the index registration too, so the catalog never exposes an
// index missing its entries. Fixing the data and re-migrating then succeeds
// and the constraint is live.
func TestUniqueBackfillRejectsDuplicates(t *testing.T) {
	ctx := context.Background()
	db := frontend.Open(memStore(t))
	migrateTo(t, db, ctx, trackerV1)

	for _, id := range []string{"u1", "u2"} {
		if err := db.Insert(ctx, "users", lir.Row{"id": lir.Text(id), "name": lir.Text("dup")}); err != nil {
			t.Fatal(err)
		}
	}

	uniqueName := `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true, default: uuid() }
      - { name: name, type: string, unique: true }
  - name: tasks
    columns:
      - { name: id,      type: string, pk: true, default: uuid() }
      - { name: user_id, type: string, ref: users.id }
      - { name: title,   type: string }
      - { name: done,    type: bool, default: false }
`
	_, err := db.MigrateFile(ctx, "schema.rad", []byte(uniqueName))
	if err == nil {
		t.Fatal("unique backfill over duplicates succeeded")
	}

	// The registration must have rolled back with the backfill: no index on
	// users. A registered-but-empty index would let reads silently drop rows.
	tables, err := db.Tables(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for _, tbl := range tables {
		if tbl.Name == "users" && len(tbl.Indexes) != 0 {
			t.Fatalf("failed backfill left index registered on users: %+v", tbl.Indexes)
		}
	}

	// Fix the data; the same migration now applies and the constraint is live.
	if _, _, err := db.Update(ctx, "users", lir.Row{"id": lir.Text("u2")}, lir.Row{"name": lir.Text("unique")}); err != nil {
		t.Fatal(err)
	}
	if steps := migrateTo(t, db, ctx, uniqueName); len(steps) == 0 {
		t.Fatal("re-migration after fixing data applied no steps")
	}
	if err := db.Insert(ctx, "users", lir.Row{"id": lir.Text("u3"), "name": lir.Text("dup")}); err == nil {
		t.Fatal("unique index registered by re-migration is not enforcing")
	}
}
