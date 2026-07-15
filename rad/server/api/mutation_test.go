package api

// Wire-level tests for mutation statements over /execute: create, update, and
// delete consuming relations, read-your-writes across statements, the
// statement-boundary semantic upgrades (a self-referential create batch, a
// unique-value swap), and the strict miss/ambiguity rules.

import (
	"context"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// tcol builds rows columns tersely; a trailing "?" on the type marks the
// column nullable ("text?").
func tcol(pairs ...string) []lirwire.RowsColumn {
	out := make([]lirwire.RowsColumn, 0, len(pairs)/2)
	for i := 0; i < len(pairs); i += 2 {
		typ, nullable := strings.CutSuffix(pairs[i+1], "?")
		col := lirwire.RowsColumn{Name: pairs[i], Type: typ}
		if nullable {
			col.Nullable = ptrBool(true)
		}
		out = append(out, col)
	}
	return out
}

// rowsRel is a constant relation of literal rows — the common mutation input.
func rowsRel(scope string, cols []lirwire.RowsColumn, rows [][]lirwire.Value) lirwire.Query {
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			scope: lirwire.Rows(scope, cols, rows),
		},
		Root: lirwire.Root{Node: scope, Cardinality: "many"},
	}
}

// scanOrdered scans a table ordered by a column — a self-contained query body.
func scanOrdered(table, scope, orderCol string) lirwire.Query {
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			scope: lirwire.Scan(table, scope),
			"o":   lirwire.Order(scope, []lirwire.OrderTerm{{Expr: lirwire.Col(scope, orderCol)}}),
		},
		Root: lirwire.Root{Node: "o", Cardinality: "many"},
	}
}

// A create statement inserts a relation's rows and returns them with defaults
// applied; a following query in the same program sees the writes.
func TestExecuteCreateReadYourWrites(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := pirwire.Prog("all",
		pirwire.Create("added", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text", "age", "int64"),
			[][]lirwire.Value{{mustValue("u1"), mustValue("Ada"), mustValue(36)}, {mustValue("u2"), mustValue("Bob"), mustValue(41)}}))),
		pirwire.Query("all", relBytes(scanOrdered("users", "u", "id"))),
	)
	res, err := c.Execute(ctx, prog)
	if err != nil {
		t.Fatal(err)
	}
	rows, _ := res.Result.([]any)
	if len(rows) != 2 {
		t.Fatalf("query after create saw %d rows, want 2", len(rows))
	}
	if res.Statements[0].Affected != 2 {
		t.Fatalf("create affected = %d, want 2", res.Statements[0].Affected)
	}
}

// A create batch whose rows reference each other through a foreign key is
// valid when the post-statement state satisfies it — statement-boundary
// constraint checking, not per-row.
func TestExecuteCreateSelfReferentialBatch(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	// employees(id pk, manager_id -> employees.id, nullable)
	if _, err := c.TableCreate(ctx, protocol.TableDef{
		Name: "employees",
		Columns: []protocol.ColumnDef{
			{Name: "id", Type: "text"},
			{Name: "manager_id", Type: "text", Nullable: true},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []protocol.ForeignKeyDef{
			{Name: "employees_manager_fk", Columns: []string{"manager_id"}, RefTable: "employees", RefColumns: []string{"id"}},
		},
	}); err != nil {
		t.Fatal(err)
	}

	// Alice manages Bob; both created in one statement. Per-row checking on
	// Bob-first would fail; the batch check sees both.
	prog := pirwire.Prog("",
		pirwire.Create("hires", "employees", relBytes(rowsRel("r",
			tcol("id", "text", "manager_id", "text?"),
			[][]lirwire.Value{{mustValue("bob"), mustValue("alice")}, {mustValue("alice"), mustValue(nil)}}))),
	)
	res, err := c.Execute(ctx, prog)
	if err != nil {
		t.Fatalf("self-referential batch should succeed: %v", err)
	}
	if res.Statements[0].Affected != 2 {
		t.Fatalf("affected = %d, want 2", res.Statements[0].Affected)
	}
}

// A single update statement may swap two unique values: only the end state is
// checked, so A and B exchanging emails is valid.
func TestExecuteUpdateUniqueSwap(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	// Seed two users with unique names.
	if _, err := c.Execute(ctx, pirwire.Prog("",
		pirwire.Create("seed", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"),
			[][]lirwire.Value{{mustValue("u1"), mustValue("ada")}, {mustValue("u2"), mustValue("bob")}}))),
	)); err != nil {
		t.Fatal(err)
	}

	// users.name is unique in the test schema. Swap the two names in one
	// statement — the rows relation supplies (id, name) post-images.
	prog := pirwire.Prog("",
		pirwire.Update("swap", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"),
			[][]lirwire.Value{{mustValue("u1"), mustValue("bob")}, {mustValue("u2"), mustValue("ada")}}))),
	)
	res, err := c.Execute(ctx, prog)
	if err != nil {
		t.Fatalf("unique swap should succeed: %v", err)
	}
	if res.Statements[0].Affected != 2 {
		t.Fatalf("affected = %d, want 2", res.Statements[0].Affected)
	}

	// Confirm the swap landed.
	got, found, err := c.Get(ctx, "users", map[string]any{"id": "u1"})
	if err != nil || !found {
		t.Fatalf("get u1: found=%v err=%v", found, err)
	}
	if got["name"] != "bob" {
		t.Fatalf("u1 name = %v, want bob", got["name"])
	}
}

// A delete statement removes rows identified by a relation's primary keys and
// returns their pre-images. A query-driven delete: delete every user, driven
// by a scan of the table's ids.
func TestExecuteDeleteFromQuery(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	if _, err := c.Execute(ctx, pirwire.Prog("",
		pirwire.Create("seed", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"),
			[][]lirwire.Value{{mustValue("u1"), mustValue("ada")}, {mustValue("u2"), mustValue("bob")}, {mustValue("u3"), mustValue("cy")}}))),
	)); err != nil {
		t.Fatal(err)
	}

	// Project each user down to just its id, then delete by that relation.
	idsOf := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"u": lirwire.Scan("users", "u"),
			"p": lirwire.Project("u", "", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("u", "id")}}),
		},
		Root: lirwire.Root{Node: "p", Cardinality: "many"},
	}
	prog := pirwire.Prog("",
		pirwire.Delete("gone", "users", relBytes(idsOf)),
	)
	res, err := c.Execute(ctx, prog)
	if err != nil {
		t.Fatal(err)
	}
	if res.Statements[0].Affected != 3 {
		t.Fatalf("deleted %d, want 3", res.Statements[0].Affected)
	}
	if tables, err := c.Query(ctx, scanOrdered("users", "u", "id")); err != nil || len(tables) != 0 {
		t.Fatalf("users after delete: %v err=%v", tables, err)
	}
}

// The composed workflow: create a user, then create a post referencing it by
// consuming the create's result relation through a ref. Read-your-writes plus
// statement-result binding across a mutation.
func TestExecuteCreateThenReferenceResult(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	// A post referencing the just-created user's id, taken from the create
	// statement's result relation.
	prog := pirwire.Prog("post",
		pirwire.Create("author", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"),
			[][]lirwire.Value{{mustValue("u1"), mustValue("Ada")}}))),
		// project the created user's id into a post row
		pirwire.Create("post", "posts", relBytes(lirwire.Query{
			Nodes: map[string]lirwire.Node{
				"a": lirwire.Ref("author", "a"),
				"p": lirwire.Project("a", "", nil, []lirwire.Field{
					{As: "id", Expr: lirwire.LitOf("p1")},
					{As: "user_id", Expr: lirwire.Col("a", "id")},
					{As: "title", Expr: lirwire.LitOf("hello")},
				}),
			},
			Root: lirwire.Root{Node: "p", Cardinality: "many"},
		})),
	)
	res, err := c.Execute(ctx, prog)
	if err != nil {
		t.Fatalf("compose create->create: %v", err)
	}
	if res.Statements[1].Affected != 1 {
		t.Fatalf("post create affected = %d, want 1", res.Statements[1].Affected)
	}
}

// An update whose input identifies a nonexistent row fails the whole program.
func TestExecuteUpdateMissFails(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := pirwire.Prog("",
		pirwire.Update("miss", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"),
			[][]lirwire.Value{{mustValue("ghost"), mustValue("nobody")}}))),
	)
	_, err := c.Execute(ctx, prog)
	if err == nil {
		t.Fatal("update of a nonexistent row should fail")
	}
}

// A delete input identifying the same row twice is rejected — the same
// strict invariant as update, exposing accidental join multiplication.
func TestExecuteDeleteAmbiguousFails(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	if _, err := c.Execute(ctx, pirwire.Prog("",
		pirwire.Create("seed", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"), [][]lirwire.Value{{mustValue("u1"), mustValue("ada")}}))),
	)); err != nil {
		t.Fatal(err)
	}
	prog := pirwire.Prog("",
		pirwire.Delete("dup", "users", relBytes(rowsRel("r",
			tcol("id", "text"), [][]lirwire.Value{{mustValue("u1")}, {mustValue("u1")}}))),
	)
	_, err := c.Execute(ctx, prog)
	assertProblem(t, err, protocol.CodeInvalid, "same row twice")
}

// An update input identifying the same row twice is an ambiguous request.
func TestExecuteUpdateAmbiguousFails(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	if _, err := c.Execute(ctx, pirwire.Prog("",
		pirwire.Create("seed", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"), [][]lirwire.Value{{mustValue("u1"), mustValue("ada")}}))),
	)); err != nil {
		t.Fatal(err)
	}
	prog := pirwire.Prog("",
		pirwire.Update("dup", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"),
			[][]lirwire.Value{{mustValue("u1"), mustValue("x")}, {mustValue("u1"), mustValue("y")}}))),
	)
	_, err := c.Execute(ctx, prog)
	assertProblem(t, err, protocol.CodeInvalid, "same row twice")
}

// A whole program rolls back on a failing statement: nothing the earlier
// statements did becomes visible.
func TestExecuteProgramAtomicRollback(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := pirwire.Prog("ok",
		pirwire.Create("ok", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"), [][]lirwire.Value{{mustValue("u1"), mustValue("ada")}}))),
		pirwire.Update("boom", "users", relBytes(rowsRel("r",
			tcol("id", "text", "name", "text"), [][]lirwire.Value{{mustValue("ghost"), mustValue("x")}}))),
	)
	if _, err := c.Execute(ctx, prog); err == nil {
		t.Fatal("program with a failing statement should fail")
	}
	// The first statement's create must have rolled back.
	if rows, err := c.Query(ctx, scanOrdered("users", "u", "id")); err != nil || len(rows) != 0 {
		t.Fatalf("rollback left rows: %v err=%v", rows, err)
	}
}
