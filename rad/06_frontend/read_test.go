package frontend_test

// Shaped reads (QIR): filters with comparison/boolean operators, ordering,
// pagination, and nested includes producing the database's native result
// shape — nested JSON, never a flattened join.

import (
	"context"
	"encoding/json"
	"testing"

	lir "rad/rad/03_lir"
	frontend "rad/rad/06_frontend"
)

// trackerDB builds a mini task tracker: boards -> tasks -> comments, tasks
// optionally assigned to users.
func trackerDB(t *testing.T) (*frontend.DB, context.Context) {
	t.Helper()
	ctx := context.Background()
	db := frontend.Open(memStore(t))
	if _, err := db.MigrateFile(ctx, "tracker.rad", []byte(`
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string }
  - name: boards
    columns:
      - { name: id,   type: string, pk: true }
      - { name: name, type: string }
  - name: tasks
    columns:
      - { name: id,          type: string, pk: true }
      - { name: board_id,    type: string, ref: boards.id }
      - { name: assignee_id, type: string, nullable: true, ref: users.id, index: true }
      - { name: title,       type: string }
      - { name: priority,    type: int64, default: 2 }
      - { name: done,        type: bool, default: false }
    indexes:
      - { columns: [board_id, done] }
  - name: comments
    columns:
      - { name: id,      type: string, pk: true }
      - { name: task_id, type: string, ref: tasks.id, index: true }
      - { name: body,    type: string }
`)); err != nil {
		t.Fatal(err)
	}

	insert := func(table string, row lir.Row) {
		t.Helper()
		if err := db.Insert(ctx, table, row); err != nil {
			t.Fatal(err)
		}
	}
	insert("users", lir.Row{"id": lir.Text("ada"), "name": lir.Text("Ada")})
	insert("users", lir.Row{"id": lir.Text("bob"), "name": lir.Text("Bob")})
	insert("boards", lir.Row{"id": lir.Text("b1"), "name": lir.Text("Launch")})
	insert("boards", lir.Row{"id": lir.Text("b2"), "name": lir.Text("Backlog")})

	insert("tasks", lir.Row{"id": lir.Text("t1"), "board_id": lir.Text("b1"), "title": lir.Text("write spec"), "priority": lir.Int64(1), "done": lir.Bool(true), "assignee_id": lir.Text("ada")})
	insert("tasks", lir.Row{"id": lir.Text("t2"), "board_id": lir.Text("b1"), "title": lir.Text("build it"), "priority": lir.Int64(1), "assignee_id": lir.Text("bob")})
	insert("tasks", lir.Row{"id": lir.Text("t3"), "board_id": lir.Text("b1"), "title": lir.Text("ship it"), "priority": lir.Int64(3)})
	insert("tasks", lir.Row{"id": lir.Text("t4"), "board_id": lir.Text("b2"), "title": lir.Text("someday"), "priority": lir.Int64(5)})

	insert("comments", lir.Row{"id": lir.Text("c1"), "task_id": lir.Text("t2"), "body": lir.Text("on it")})
	insert("comments", lir.Row{"id": lir.Text("c2"), "task_id": lir.Text("t2"), "body": lir.Text("done soon")})
	return db, ctx
}

func TestReadFilterOperators(t *testing.T) {
	db, ctx := trackerDB(t)

	col := func(c string) lir.Expr { return lir.ColRef{Column: c} }
	lit := func(v lir.Value) lir.Expr { return lir.Literal{Value: v} }

	cases := []struct {
		name   string
		filter lir.Expr
		want   int
	}{
		{"equality", lir.Eq{Left: col("board_id"), Right: lit(lir.Text("b1"))}, 3},
		{"not equal", lir.Cmp{Op: lir.OpNe, Left: col("board_id"), Right: lit(lir.Text("b1"))}, 1},
		{"less than", lir.Cmp{Op: lir.OpLt, Left: col("priority"), Right: lit(lir.Int64(3))}, 2},
		{"at least", lir.Cmp{Op: lir.OpGte, Left: col("priority"), Right: lit(lir.Int64(3))}, 2},
		{"and", lir.And{Exprs: []lir.Expr{
			lir.Eq{Left: col("board_id"), Right: lit(lir.Text("b1"))},
			lir.Eq{Left: col("done"), Right: lit(lir.Bool(false))},
		}}, 2},
		{"or", lir.Or{Exprs: []lir.Expr{
			lir.Eq{Left: col("priority"), Right: lit(lir.Int64(5))},
			lir.Eq{Left: col("done"), Right: lit(lir.Bool(true))},
		}}, 2},
		{"not", lir.Not{Expr: lir.Eq{Left: col("board_id"), Right: lit(lir.Text("b1"))}}, 1},
		// NULL never matches comparisons: only assigned tasks equal a user.
		{"null excluded", lir.Cmp{Op: lir.OpNe, Left: col("assignee_id"), Right: lit(lir.Text("nobody"))}, 2},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			recs, err := db.Read(ctx, lir.Read{Table: "tasks", Filter: tc.filter})
			if err != nil {
				t.Fatal(err)
			}
			if len(recs) != tc.want {
				t.Fatalf("got %d rows, want %d", len(recs), tc.want)
			}
		})
	}
}

func TestReadOrderAndPaginate(t *testing.T) {
	db, ctx := trackerDB(t)

	recs, err := db.Read(ctx, lir.Read{
		Table:   "tasks",
		OrderBy: []lir.OrderTerm{{Column: "priority", Desc: true}, {Column: "id"}},
		Offset:  1,
		Limit:   2,
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(recs) != 2 {
		t.Fatalf("got %d rows", len(recs))
	}
	// Full order: t4(5), t3(3), then priority-1 ties t1,t2 by id.
	// Offset 1, limit 2 → t3, t1.
	if !recs[0].Columns["id"].Equal(lir.Text("t3")) || !recs[1].Columns["id"].Equal(lir.Text("t1")) {
		t.Fatalf("got %v, %v", recs[0].Columns["id"], recs[1].Columns["id"])
	}
}

// The planner picks access paths from the filter: complete PK equality is a
// point lookup, an indexed equality prefix is an index scan — visible here
// only through correct results, asserted directly in the planner tests.
func TestReadAccessPaths(t *testing.T) {
	db, ctx := trackerDB(t)

	byPK, err := db.Read(ctx, lir.Read{
		Table:  "tasks",
		Filter: lir.Eq{Left: lir.ColRef{Column: "id"}, Right: lir.Literal{Value: lir.Text("t2")}},
	})
	if err != nil || len(byPK) != 1 {
		t.Fatalf("pk lookup: %d rows err=%v", len(byPK), err)
	}

	byIndex, err := db.Read(ctx, lir.Read{
		Table: "tasks",
		Filter: lir.And{Exprs: []lir.Expr{
			lir.Eq{Left: lir.ColRef{Column: "board_id"}, Right: lir.Literal{Value: lir.Text("b1")}},
			lir.Eq{Left: lir.ColRef{Column: "done"}, Right: lir.Literal{Value: lir.Bool(false)}},
		}},
	})
	if err != nil || len(byIndex) != 2 {
		t.Fatalf("index scan: %d rows err=%v", len(byIndex), err)
	}
}

// Includes assemble the nested result shape: parents as objects (null for
// NULL FKs), children as arrays, recursively.
func TestReadNestedIncludes(t *testing.T) {
	db, ctx := trackerDB(t)

	recs, err := db.Read(ctx, lir.Read{
		Table:  "boards",
		Filter: lir.Eq{Left: lir.ColRef{Column: "id"}, Right: lir.Literal{Value: lir.Text("b1")}},
		Include: []lir.Include{{
			FK: "tasks_board_id_fk", Dir: lir.ToChildren, As: "tasks",
			OrderBy: []lir.OrderTerm{{Column: "id"}},
			Include: []lir.Include{
				{FK: "tasks_assignee_id_fk", Dir: lir.ToParent, As: "assignee"},
				{FK: "comments_task_id_fk", Dir: lir.ToChildren, As: "comments", OrderBy: []lir.OrderTerm{{Column: "id"}}},
			},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(recs) != 1 {
		t.Fatalf("got %d boards", len(recs))
	}
	tasks := recs[0].Children["tasks"]
	if len(tasks) != 3 {
		t.Fatalf("got %d tasks", len(tasks))
	}

	// t1: assigned to Ada, no comments.
	if got := tasks[0].Parents["assignee"]; got == nil || !got.Columns["name"].Equal(lir.Text("Ada")) {
		t.Fatalf("t1 assignee: %v", got)
	}
	// t2: two comments, nested two levels deep.
	if got := tasks[1].Children["comments"]; len(got) != 2 || !got[0].Columns["body"].Equal(lir.Text("on it")) {
		t.Fatalf("t2 comments: %v", got)
	}
	// t3: unassigned — the parent include is nil, not missing.
	if got, present := tasks[2].Parents["assignee"]; !present || got != nil {
		t.Fatalf("t3 assignee: present=%v got=%v", present, got)
	}
}

// Children includes support their own filter, order, and limit.
func TestReadChildShaping(t *testing.T) {
	db, ctx := trackerDB(t)

	recs, err := db.Read(ctx, lir.Read{
		Table:  "boards",
		Filter: lir.Eq{Left: lir.ColRef{Column: "id"}, Right: lir.Literal{Value: lir.Text("b1")}},
		Include: []lir.Include{{
			FK: "tasks_board_id_fk", Dir: lir.ToChildren, As: "open_tasks",
			Filter:  lir.Eq{Left: lir.ColRef{Column: "done"}, Right: lir.Literal{Value: lir.Bool(false)}},
			OrderBy: []lir.OrderTerm{{Column: "priority"}},
			Limit:   1,
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	open := recs[0].Children["open_tasks"]
	if len(open) != 1 || !open[0].Columns["id"].Equal(lir.Text("t2")) {
		t.Fatalf("open tasks: %v", open)
	}
}

// ReadJSON produces plain nested JSON — the wire format of the database.
func TestReadJSONShape(t *testing.T) {
	db, ctx := trackerDB(t)

	rows, err := db.ReadJSON(ctx, lir.Read{
		Table:  "tasks",
		Filter: lir.Eq{Left: lir.ColRef{Column: "id"}, Right: lir.Literal{Value: lir.Text("t2")}},
		Include: []lir.Include{
			{FK: "tasks_assignee_id_fk", Dir: lir.ToParent, As: "assignee"},
			{FK: "comments_task_id_fk", Dir: lir.ToChildren, As: "comments", OrderBy: []lir.OrderTerm{{Column: "id"}}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	raw, err := json.Marshal(rows)
	if err != nil {
		t.Fatal(err)
	}

	var decoded []struct {
		ID       string `json:"id"`
		Title    string `json:"title"`
		Priority int64  `json:"priority"`
		Done     bool   `json:"done"`
		Assignee *struct {
			Name string `json:"name"`
		} `json:"assignee"`
		Comments []struct {
			Body string `json:"body"`
		} `json:"comments"`
	}
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	got := decoded[0]
	if got.ID != "t2" || got.Priority != 1 || got.Done || got.Assignee == nil || got.Assignee.Name != "Bob" || len(got.Comments) != 2 {
		t.Fatalf("shape wrong: %s", raw)
	}
}

// Read validation errors come from the planner before any data access.
func TestReadValidation(t *testing.T) {
	db, ctx := trackerDB(t)

	if _, err := db.Read(ctx, lir.Read{Table: "ghost"}); err == nil {
		t.Error("unknown table accepted")
	}
	if _, err := db.Read(ctx, lir.Read{Table: "tasks", Filter: lir.Eq{Left: lir.ColRef{Column: "ghost"}, Right: lir.Literal{Value: lir.Int64(1)}}}); err == nil {
		t.Error("unknown filter column accepted")
	}
	if _, err := db.Read(ctx, lir.Read{Table: "tasks", OrderBy: []lir.OrderTerm{{Column: "ghost"}}}); err == nil {
		t.Error("unknown order column accepted")
	}
	if _, err := db.Read(ctx, lir.Read{Table: "tasks", Include: []lir.Include{{FK: "nope", Dir: lir.ToParent, As: "x"}}}); err == nil {
		t.Error("unknown fk accepted")
	}
	if _, err := db.Read(ctx, lir.Read{Table: "boards", Include: []lir.Include{{FK: "tasks_board_id_fk", Dir: lir.ToChildren}}}); err == nil {
		t.Error("include without As accepted")
	}
}
