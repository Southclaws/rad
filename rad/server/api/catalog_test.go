package api

// Wire-level contract tests for the imperative catalog operations: the
// radclient speaking to real handlers over real HTTP on an in-memory
// SlateDB. Covers the full table/column/index lifecycle, definition
// validation and atomicity, backfill conflicts, referential guards, and the
// catalog mode gate.

import (
	"context"
	"errors"
	"net/http/httptest"
	"strings"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// testServerInMode is testServer with the catalog mode stamped before the
// handler is built, the way serve boots a real database.
func testServerInMode(t *testing.T, mode catalog.Mode) *radclient.Client {
	t.Helper()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })

	cat := catalog.New(store)
	if _, err := cat.InitMode(context.Background(), mode); err != nil {
		t.Fatal(err)
	}
	handler, err := New(frontend.Open(store), cat)
	if err != nil {
		t.Fatal(err)
	}
	srv := httptest.NewServer(handler)
	t.Cleanup(srv.Close)

	c, err := radclient.Dial("rad://" + strings.TrimPrefix(srv.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// expectInvalid asserts err is an invalid problem whose detail mentions want.
func expectInvalid(t *testing.T, err error, want string) {
	t.Helper()
	var ae *radclient.APIError
	if !errors.As(err, &ae) {
		t.Fatalf("err = %v, want an API error", err)
	}
	if ae.Problem.Code != protocol.CodeInvalid {
		t.Fatalf("code = %q, want invalid (%v)", ae.Problem.Code, err)
	}
	if !strings.Contains(ae.Problem.Detail, want) {
		t.Fatalf("detail %q should mention %q", ae.Problem.Detail, want)
	}
}

// trackerDef is a full single-call definition: columns with generator and
// literal defaults, a composite-friendly primary key, a secondary index, and
// a self-referential foreign key.
func trackerDef() protocol.TableDef {
	return protocol.TableDef{
		Name: "tasks",
		Columns: []protocol.ColumnDef{
			{Name: "id", Type: "text", Default: &protocol.ColumnDefault{Func: "uuid"}},
			{Name: "title", Type: "text"},
			{Name: "status", Type: "text", Default: &protocol.ColumnDefault{Value: "open"}},
			{Name: "points", Type: "int64", Default: &protocol.ColumnDefault{Value: 1}},
			{Name: "parent_id", Type: "text", Nullable: true},
		},
		PrimaryKey: []string{"id"},
		Indexes: []protocol.IndexDef{
			{Name: "tasks_status_idx", Columns: []string{"status"}},
		},
		ForeignKeys: []protocol.ForeignKeyDef{
			{Name: "tasks_parent_fk", Columns: []string{"parent_id"}, RefTable: "tasks", RefColumns: []string{"id"}},
		},
	}
}

// One HTTP call creates the whole table: columns, defaults, primary key,
// index, and a self-referential foreign key — and introspection returns the
// definition back, fully resolved.
func TestTableCreateWholeDefinitionInOneCall(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	info, err := c.TableCreate(ctx, trackerDef())
	if err != nil {
		t.Fatal(err)
	}
	if info.Name != "tasks" || len(info.Columns) != 5 {
		t.Fatalf("info = %+v", info)
	}
	if len(info.Indexes) != 1 || info.Indexes[0].Name != "tasks_status_idx" {
		t.Fatalf("indexes = %+v", info.Indexes)
	}
	if len(info.ForeignKeys) != 1 || info.ForeignKeys[0].RefTable != "tasks" {
		t.Fatalf("foreign keys should resolve the self-reference by name: %+v", info.ForeignKeys)
	}

	// Defaults round-trip: generator by func, literals by typed value.
	byName := map[string]protocol.ColumnInfo{}
	for _, col := range info.Columns {
		byName[col.Name] = col
	}
	if d := byName["id"].Default; d == nil || d.Func != "uuid" {
		t.Fatalf("id default = %+v, want func uuid", byName["id"].Default)
	}
	if d := byName["status"].Default; d == nil || d.Value != "open" {
		t.Fatalf("status default = %+v, want literal \"open\"", byName["status"].Default)
	}
	if d := byName["points"].Default; d == nil || d.Value == nil {
		t.Fatalf("points default = %+v, want literal 1", byName["points"].Default)
	}

	// The table is immediately usable: defaults fill omitted columns.
	task, err := c.Create(ctx, "tasks", map[string]any{"title": "write tests"})
	if err != nil {
		t.Fatal(err)
	}
	if task["id"] == "" || task["status"] != "open" {
		t.Fatalf("stored task = %v", task)
	}

	// TableList reports the same shape as the mutation response.
	tables, err := c.Tables(ctx)
	if err != nil || len(tables) != 1 {
		t.Fatalf("tables=%v err=%v", tables, err)
	}
	if len(tables[0].Indexes) != 1 || len(tables[0].ForeignKeys) != 1 {
		t.Fatalf("introspection lost definitions: %+v", tables[0])
	}
}

// A rejected definition commits nothing — the name stays free — and each
// validation rule maps to an invalid problem over the wire.
func TestTableCreateValidationAndAtomicity(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	col := func(name, typ string) protocol.ColumnDef { return protocol.ColumnDef{Name: name, Type: typ} }
	base := func() protocol.TableDef {
		return protocol.TableDef{
			Name:       "things",
			Columns:    []protocol.ColumnDef{col("id", "text"), col("n", "int64")},
			PrimaryKey: []string{"id"},
		}
	}

	cases := []struct {
		name   string
		mutate func(*protocol.TableDef)
		detail string
	}{
		{"missing name", func(d *protocol.TableDef) { d.Name = "" }, "name is required"},
		{"duplicate column", func(d *protocol.TableDef) { d.Columns = append(d.Columns, col("n", "int64")) }, "duplicate column"},
		{"unsupported type", func(d *protocol.TableDef) { d.Columns[1].Type = "varchar" }, "unsupported type"},
		{"missing primary key", func(d *protocol.TableDef) { d.PrimaryKey = nil }, "needs a primary key"},
		{"unknown pk column", func(d *protocol.TableDef) { d.PrimaryKey = []string{"ghost"} }, "does not exist"},
		{"nullable pk column", func(d *protocol.TableDef) { d.Columns[0].Nullable = true }, "must not be nullable"},
		{"index over unknown column", func(d *protocol.TableDef) {
			d.Indexes = []protocol.IndexDef{{Name: "bad", Columns: []string{"ghost"}}}
		}, "unknown column"},
		{"fk to unknown table", func(d *protocol.TableDef) {
			d.ForeignKeys = []protocol.ForeignKeyDef{{Name: "fk", Columns: []string{"id"}, RefTable: "ghosts", RefColumns: []string{"id"}}}
		}, "unknown table"},
		{"unknown default func", func(d *protocol.TableDef) {
			d.Columns[1].Default = &protocol.ColumnDefault{Func: "sequence"}
		}, "unknown default function"},
		{"generator on wrong type", func(d *protocol.TableDef) {
			d.Columns[1].Default = &protocol.ColumnDefault{Func: "uuid"}
		}, "uuid() default requires a string column"},
		{"default value type mismatch", func(d *protocol.TableDef) {
			d.Columns[1].Default = &protocol.ColumnDefault{Value: "ten"}
		}, "expects a number"},
		{"default with func and value", func(d *protocol.TableDef) {
			d.Columns[1].Default = &protocol.ColumnDefault{Func: "now_ms", Value: 5}
		}, "both func and value"},
		{"default with neither", func(d *protocol.TableDef) {
			d.Columns[1].Default = &protocol.ColumnDefault{}
		}, "func or value"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			def := base()
			tc.mutate(&def)
			_, err := c.TableCreate(ctx, def)
			if err == nil {
				t.Fatal("definition should be rejected")
			}
			expectInvalid(t, err, tc.detail)
		})
	}

	// Nothing stuck: the name is free and the valid definition works.
	if _, err := c.TableCreate(ctx, base()); err != nil {
		t.Fatalf("valid definition after rejections: %v", err)
	}
}

// The full lifecycle: update the table's name, create/update/delete columns,
// create/delete indexes — each response reflecting the post-change table.
func TestCatalogLifecycleOverTheWire(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	created, err := c.TableCreate(ctx, protocol.TableDef{
		Name: "notes",
		Columns: []protocol.ColumnDef{
			{Name: "id", Type: "int64"},
			{Name: "body", Type: "text"},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if created.ID != 1 || len(created.Columns) != 2 || created.Columns[0].ID != 1 || created.Columns[1].ID != 2 {
		t.Fatalf("allocated schema IDs: %+v", created)
	}
	if _, err := c.Create(ctx, "notes", map[string]any{"id": 1, "body": "first"}); err != nil {
		t.Fatal(err)
	}

	// Table rename via update; the old name stops resolving.
	info, err := c.TableUpdate(ctx, "notes", "memos")
	if err != nil || info.Name != "memos" || info.ID != created.ID || info.Columns[1].ID != created.Columns[1].ID {
		t.Fatalf("info=%+v err=%v", info, err)
	}
	if _, err := c.Create(ctx, "notes", map[string]any{"id": 2, "body": "x"}); err == nil {
		t.Fatal("old table name still accepts writes")
	}

	// The data survived the rename.
	if row, found, err := c.Get(ctx, "memos", map[string]any{"id": 1}); err != nil || !found || row["body"] != "first" {
		t.Fatalf("row=%v found=%v err=%v", row, found, err)
	}

	// Column create: needs nullable or a literal default on a live table.
	if _, err := c.ColumnCreate(ctx, "memos", protocol.ColumnDef{Name: "pinned", Type: "bool"}); err == nil {
		t.Fatal("non-nullable, defaultless column on a live table should be rejected")
	}
	info, err = c.ColumnCreate(ctx, "memos", protocol.ColumnDef{
		Name: "pinned", Type: "bool", Default: &protocol.ColumnDefault{Value: false},
	})
	if err != nil || len(info.Columns) != 3 || info.Columns[2].ID != 3 {
		t.Fatalf("info=%+v err=%v", info, err)
	}

	// Column update (rename) rewrites references and keeps data readable.
	if _, err := c.ColumnUpdate(ctx, "memos", "body", "content"); err != nil {
		t.Fatal(err)
	}
	if row, found, err := c.Get(ctx, "memos", map[string]any{"id": 1}); err != nil || !found || row["content"] != "first" {
		t.Fatalf("renamed column lost data: row=%v found=%v err=%v", row, found, err)
	}

	// Index create on a populated table backfills and serves reads.
	info, err = c.IndexCreate(ctx, "memos", protocol.IndexDef{Name: "memos_content_idx", Columns: []string{"content"}})
	if err != nil || len(info.Indexes) != 1 {
		t.Fatalf("info=%+v err=%v", info, err)
	}
	rows, err := c.Query(ctx, filteredQ("memos",
		lirwire.Binary("eq", lirwire.Col("t", "content"), lirwire.LitOf("first"))))
	if err != nil || len(rows) != 1 {
		t.Fatalf("indexed read rows=%v err=%v", rows, err)
	}

	// Column delete: guarded while indexed, fine after the index goes.
	if _, err := c.ColumnDelete(ctx, "memos", "content"); err == nil {
		t.Fatal("deleting an indexed column should be rejected")
	}
	if _, err := c.IndexDelete(ctx, "memos", "memos_content_idx"); err != nil {
		t.Fatal(err)
	}
	info, err = c.ColumnDelete(ctx, "memos", "content")
	if err != nil || len(info.Columns) != 2 {
		t.Fatalf("info=%+v err=%v", info, err)
	}

	// Table delete; the catalog is empty again.
	if err := c.TableDelete(ctx, "memos"); err != nil {
		t.Fatal(err)
	}
	if tables, err := c.Tables(ctx); err != nil || len(tables) != 0 {
		t.Fatalf("tables=%v err=%v", tables, err)
	}
}

// Backfilling a unique index over duplicate data is an invalid request —
// retrying cannot succeed until the data changes — and rolls the
// registration back; after the duplicate goes, the same request succeeds
// and the constraint holds.
func TestIndexCreateUniqueBackfillConflict(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	if _, err := c.TableCreate(ctx, protocol.TableDef{
		Name: "users",
		Columns: []protocol.ColumnDef{
			{Name: "id", Type: "int64"},
			{Name: "email", Type: "text"},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	for id, email := range map[int]string{1: "a@x.com", 2: "dupe@x.com", 3: "dupe@x.com"} {
		if _, err := c.Create(ctx, "users", map[string]any{"id": id, "email": email}); err != nil {
			t.Fatal(err)
		}
	}

	_, err := c.IndexCreate(ctx, "users", protocol.IndexDef{Name: "users_email_uq", Columns: []string{"email"}, Unique: true})
	if err == nil {
		t.Fatal("unique backfill over duplicates should fail")
	}
	expectInvalid(t, err, "share a value")
	tables, err := c.Tables(ctx)
	if err != nil || len(tables) != 1 || len(tables[0].Indexes) != 0 {
		t.Fatalf("failed backfill must not register the index: %+v err=%v", tables, err)
	}

	if _, err := c.Delete(ctx, "users", map[string]any{"id": 3}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.IndexCreate(ctx, "users", protocol.IndexDef{Name: "users_email_uq", Columns: []string{"email"}, Unique: true}); err != nil {
		t.Fatalf("retry after removing the duplicate: %v", err)
	}
	// The constraint is live.
	if _, err := c.Create(ctx, "users", map[string]any{"id": 4, "email": "a@x.com"}); err == nil {
		t.Fatal("unique index should reject the duplicate insert")
	}
}

// Referential guards over the wire: a referenced table cannot be deleted
// until its referencing table goes, and foreign key columns are protected.
func TestCatalogDeleteGuards(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	if _, err := c.TableCreate(ctx, protocol.TableDef{
		Name:       "authors",
		Columns:    []protocol.ColumnDef{{Name: "id", Type: "int64"}},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.TableCreate(ctx, protocol.TableDef{
		Name: "books",
		Columns: []protocol.ColumnDef{
			{Name: "id", Type: "int64"},
			{Name: "author_id", Type: "int64"},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []protocol.ForeignKeyDef{
			{Name: "books_author_fk", Columns: []string{"author_id"}, RefTable: "authors", RefColumns: []string{"id"}},
		},
	}); err != nil {
		t.Fatal(err)
	}

	err := c.TableDelete(ctx, "authors")
	expectInvalid(t, err, "books_author_fk")

	_, err = c.ColumnDelete(ctx, "books", "author_id")
	expectInvalid(t, err, "foreign key")

	_, err = c.ColumnDelete(ctx, "books", "id")
	expectInvalid(t, err, "primary key")

	// Referencing table first, then the parent.
	if err := c.TableDelete(ctx, "books"); err != nil {
		t.Fatal(err)
	}
	if err := c.TableDelete(ctx, "authors"); err != nil {
		t.Fatal(err)
	}
}

// A schema-managed database rejects every imperative catalog operation with
// one uniform invalid problem, while migration and reads stay open.
func TestSchemaManagedModeGatesCatalogOps(t *testing.T) {
	c := testServerInMode(t, catalog.ModeSchema)
	ctx := context.Background()

	if mode, err := c.Mode(ctx); err != nil || mode != "schema" {
		t.Fatalf("mode=%q err=%v", mode, err)
	}

	// Migration is the sanctioned channel and works.
	if _, err := c.Migrate(ctx, testSchema); err != nil {
		t.Fatal(err)
	}

	ops := map[string]func() error{
		"TableCreate": func() error { _, err := c.TableCreate(ctx, trackerDef()); return err },
		"TableUpdate": func() error { _, err := c.TableUpdate(ctx, "users", "people"); return err },
		"TableDelete": func() error { return c.TableDelete(ctx, "posts") },
		"ColumnCreate": func() error {
			_, err := c.ColumnCreate(ctx, "users", protocol.ColumnDef{Name: "bio", Type: "text", Nullable: true})
			return err
		},
		"ColumnUpdate": func() error { _, err := c.ColumnUpdate(ctx, "users", "name", "full_name"); return err },
		"ColumnDelete": func() error { _, err := c.ColumnDelete(ctx, "users", "age"); return err },
		"IndexCreate": func() error {
			_, err := c.IndexCreate(ctx, "users", protocol.IndexDef{Name: "users_age_idx", Columns: []string{"age"}})
			return err
		},
		"IndexDelete": func() error { _, err := c.IndexDelete(ctx, "posts", "posts_user_id_idx"); return err },
	}
	for name, op := range ops {
		t.Run(name, func(t *testing.T) {
			err := op()
			if err == nil {
				t.Fatal("operation should be rejected on a schema-managed database")
			}
			expectInvalid(t, err, "schema-managed")
		})
	}

	// The gate rejected before touching anything: the schema is unchanged.
	tables, err := c.Tables(ctx)
	if err != nil || len(tables) != 2 || tables[0].Name != "posts" || tables[1].Name != "users" {
		t.Fatalf("tables=%+v err=%v", tables, err)
	}
}

// A direct-mode database reports its mode and accepts both channels: the
// imperative operations and the schema.rad reconciler.
func TestDirectModeAcceptsBothChannels(t *testing.T) {
	c := testServerInMode(t, catalog.ModeDirect)
	ctx := context.Background()

	if mode, err := c.Mode(ctx); err != nil || mode != "direct" {
		t.Fatalf("mode=%q err=%v", mode, err)
	}
	if _, err := c.Migrate(ctx, testSchema); err != nil {
		t.Fatal(err)
	}
	if info, err := c.Info(ctx); err != nil || info.SchemaVersion != 2 {
		t.Fatalf("two direct-mode reconciliation steps: info=%+v err=%v; want v2", info, err)
	}
	if _, err := c.ColumnCreate(ctx, "users", protocol.ColumnDef{Name: "bio", Type: "text", Nullable: true}); err != nil {
		t.Fatal(err)
	}
	if info, err := c.Info(ctx); err != nil || info.SchemaVersion != 3 {
		t.Fatalf("direct column change: info=%+v err=%v; want v3", info, err)
	}
}

// Unknown targets map to invalid problems, not 500s.
func TestCatalogOpsOnUnknownTargets(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	if err := c.TableDelete(ctx, "ghost"); err == nil {
		t.Fatal("deleting an unknown table should fail")
	} else {
		expectInvalid(t, err, "does not exist")
	}
	if _, err := c.TableUpdate(ctx, "ghost", "spirit"); err == nil {
		t.Fatal("updating an unknown table should fail")
	} else {
		expectInvalid(t, err, "does not exist")
	}
	if _, err := c.ColumnDelete(ctx, "ghost", "x"); err == nil {
		t.Fatal("deleting a column of an unknown table should fail")
	} else {
		expectInvalid(t, err, "does not exist")
	}
	if _, err := c.IndexDelete(ctx, "ghost", "x"); err == nil {
		t.Fatal("deleting an index of an unknown table should fail")
	} else {
		expectInvalid(t, err, "does not exist")
	}
}
