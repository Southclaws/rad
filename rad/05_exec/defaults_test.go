package exec

// Column defaults and the bool type through the write path: omitted columns
// get generated (uuid, now_ms) or literal defaults; explicit values win.

import (
	"regexp"
	"testing"
	"time"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

func setupWithDefaults(t *testing.T) (*Engine, string) {
	t.Helper()
	eng, ctx := setup(t)
	_, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "tickets",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText, Format: "uuid", Default: &catalog.Default{Func: catalog.DefaultUUID}},
			{Name: "title", Type: catalog.TypeText},
			{Name: "status", Type: catalog.TypeText, Default: &catalog.Default{Text: "open"}},
			{Name: "priority", Type: catalog.TypeInt64, Default: &catalog.Default{Int64: 2}},
			{Name: "done", Type: catalog.TypeBool, Default: &catalog.Default{Bool: false}},
			{Name: "created_at", Type: catalog.TypeInt64, Format: "unix_ms", Default: &catalog.Default{Func: catalog.DefaultNowMS}},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "tickets_done_idx", Columns: []string{"done"}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	return eng, "tickets"
}

var uuidRE = regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`)

func TestInsertAppliesDefaults(t *testing.T) {
	eng, table := setupWithDefaults(t)
	ctx := t.Context()

	before := time.Now().UnixMilli()
	if err := eng.Insert(ctx, table, qir.Row{"title": qir.Text("fix roof")}); err != nil {
		t.Fatal(err)
	}

	rows, err := eng.ScanIndex(ctx, table, "tickets_done_idx", nil)
	if err != nil || len(rows) != 1 {
		t.Fatalf("rows=%d err=%v", len(rows), err)
	}
	row := rows[0]

	if !uuidRE.MatchString(row["id"].Text) {
		t.Errorf("id = %q, want uuid v4", row["id"].Text)
	}
	if !row["status"].Equal(qir.Text("open")) || !row["priority"].Equal(qir.Int64(2)) {
		t.Errorf("literal defaults not applied: %v", row)
	}
	if !row["done"].Equal(qir.Bool(false)) {
		t.Errorf("bool default not applied: %v", row["done"])
	}
	if at := row["created_at"].Int64; at < before || at > time.Now().UnixMilli() {
		t.Errorf("created_at = %d not in [%d, now]", at, before)
	}
}

func TestExplicitValuesBeatDefaults(t *testing.T) {
	eng, table := setupWithDefaults(t)
	ctx := t.Context()

	err := eng.Insert(ctx, table, qir.Row{
		"id": qir.Text("custom-id"), "title": qir.Text("x"),
		"status": qir.Text("closed"), "done": qir.Bool(true),
	})
	if err != nil {
		t.Fatal(err)
	}

	row, ok, err := eng.GetByPrimaryKey(ctx, table, qir.Row{"id": qir.Text("custom-id")})
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !row["status"].Equal(qir.Text("closed")) || !row["done"].Equal(qir.Bool(true)) {
		t.Errorf("explicit values overridden: %v", row)
	}
}

// uuid() defaults generate distinct primary keys — consecutive inserts
// without an id must not collide.
func TestUUIDDefaultsAreUnique(t *testing.T) {
	eng, table := setupWithDefaults(t)
	ctx := t.Context()

	for range 10 {
		if err := eng.Insert(ctx, table, qir.Row{"title": qir.Text("t")}); err != nil {
			t.Fatal(err)
		}
	}
	rows, err := eng.ScanIndex(ctx, table, "tickets_done_idx", nil)
	if err != nil || len(rows) != 10 {
		t.Fatalf("rows=%d err=%v", len(rows), err)
	}
}

// Bool participates in index keys: false entries sort before true.
func TestBoolIndexScan(t *testing.T) {
	eng, table := setupWithDefaults(t)
	ctx := t.Context()

	for _, done := range []bool{true, false, true} {
		if err := eng.Insert(ctx, table, qir.Row{"title": qir.Text("t"), "done": qir.Bool(done)}); err != nil {
			t.Fatal(err)
		}
	}

	rows, err := eng.ScanIndex(ctx, table, "tickets_done_idx", qir.Row{"done": qir.Bool(true)})
	if err != nil || len(rows) != 2 {
		t.Fatalf("done=true rows=%d err=%v", len(rows), err)
	}
	all, err := eng.ScanIndex(ctx, table, "tickets_done_idx", nil)
	if err != nil || len(all) != 3 {
		t.Fatalf("all rows=%d err=%v", len(all), err)
	}
	// false sorts before true in the index, so the first entry is undone.
	if all[0]["done"].Bool {
		t.Errorf("index order wrong: first row should be done=false")
	}
}
