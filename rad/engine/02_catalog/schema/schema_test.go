// These tests document the rad.schema.yaml format: YAML validated by an embedded
// JSON Schema, then compiled to catalog definitions. The demo application's
// real schema is parsed at the end as an integration check.
package schema_test

import (
	"os"
	"strings"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

func parse(t *testing.T, src string) *schema.Schema {
	t.Helper()
	s, err := schema.Parse("test.rad", []byte(src))
	if err != nil {
		t.Fatal(err)
	}
	return s
}

func TestColumnsTypesAndNullability(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: things
    columns:
      - { id: 1, name: id,    type: string, pk: true }
      - { id: 2, name: count, type: int64 }
      - { id: 3, name: score, type: float64, nullable: true }
      - { id: 4, name: done,  type: bool }
`)
	tbl := s.Tables[0].Def
	if tbl.Name != "things" || len(tbl.Columns) != 4 {
		t.Fatalf("parsed %+v", tbl)
	}
	want := []struct {
		name     string
		typ      catalog.Type
		nullable bool
	}{
		{"id", catalog.TypeText, false},
		{"count", catalog.TypeInt64, false},
		{"score", catalog.TypeFloat64, true},
		{"done", catalog.TypeBool, false},
	}
	for i, w := range want {
		c := tbl.Columns[i]
		if c.Name != w.name || c.Type != w.typ || c.Nullable != w.nullable {
			t.Errorf("column %d = %+v, want %+v", i, c, w)
		}
	}
	if len(tbl.PrimaryKey) != 1 || tbl.PrimaryKey[0] != "id" {
		t.Errorf("pk = %v", tbl.PrimaryKey)
	}
}

// Column-level pk flags in declaration order form a composite key;
// table-level primary_key is the explicit alternative. Mixing both is an
// error.
func TestPrimaryKeys(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: pairs
    columns:
      - { id: 1, name: a, type: string, pk: true }
      - { id: 2, name: b, type: string, pk: true }
  - id: 2
    name: triples
    columns:
      - { id: 1, name: x, type: string }
      - { id: 2, name: y, type: string }
    primary_key: [x, y]
`)
	if pk := s.Tables[0].Def.PrimaryKey; len(pk) != 2 || pk[0] != "a" || pk[1] != "b" {
		t.Errorf("column-level pk = %v", pk)
	}
	if pk := s.Tables[1].Def.PrimaryKey; len(pk) != 2 || pk[0] != "x" {
		t.Errorf("table-level pk = %v", pk)
	}

	_, err := schema.Parse("test.rad", []byte(`
tables:
  - id: 1
    name: bad
    columns:
      - { id: 1, name: a, type: string, pk: true }
    primary_key: [a]
`))
	if err == nil || !strings.Contains(err.Error(), "both column-level") {
		t.Errorf("mixed pk styles accepted: %v", err)
	}
}

// unique/index column shorthands and table-level indexes generate
// deterministically named indexes.
func TestIndexes(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: posts
    columns:
      - { id: 1, name: id,     type: string, pk: true }
      - { id: 2, name: slug,   type: string, unique: true }
      - { id: 3, name: author, type: string, index: true }
      - { id: 4, name: region, type: string }
    indexes:
      - { columns: [region, author] }
      - { columns: [region, slug], unique: true }
`)
	idx := s.Tables[0].Def.Indexes
	want := []catalog.IndexDef{
		{Name: "posts_slug_uq", Columns: []string{"slug"}, Unique: true},
		{Name: "posts_author_idx", Columns: []string{"author"}},
		{Name: "posts_region_author_idx", Columns: []string{"region", "author"}},
		{Name: "posts_region_slug_uq", Columns: []string{"region", "slug"}, Unique: true},
	}
	if len(idx) != len(want) {
		t.Fatalf("indexes = %+v", idx)
	}
	for i, w := range want {
		got := idx[i]
		if got.Name != w.Name || got.Unique != w.Unique || strings.Join(got.Columns, ",") != strings.Join(w.Columns, ",") {
			t.Errorf("index %d = %+v, want %+v", i, got, w)
		}
	}
}

// ref: table.column declares a foreign key named {table}_{column}_fk.
func TestForeignKeys(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: orders
    columns:
      - { id: 1, name: id,      type: string, pk: true }
      - { id: 2, name: user_id, type: string, ref: users.id }
`)
	fks := s.Tables[0].Def.ForeignKeys
	if len(fks) != 1 {
		t.Fatalf("fks = %+v", fks)
	}
	fk := fks[0]
	if fk.Name != "orders_user_id_fk" || fk.RefTable != "users" || fk.RefColumns[0] != "id" || fk.Columns[0] != "user_id" {
		t.Errorf("fk = %+v", fk)
	}
}

// Formats are free-form metadata; defaults accept generators and typed
// literals (YAML scalars keep their types).
func TestFormatsAndDefaults(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: all_defaults
    columns:
      - { id: 1, name: id,       type: string, pk: true, default: uuid(), format: uuid }
      - { id: 2, name: email,    type: string, nullable: true, format: email }
      - { id: 3, name: status,   type: string, default: open }
      - { id: 4, name: attempts, type: int64, default: 3 }
      - { id: 5, name: ratio,    type: float64, default: 0.5 }
      - { id: 6, name: done,     type: bool, default: false }
      - { id: 7, name: at,       type: int64, format: unix_ms, default: now_ms() }
`)
	byName := map[string]catalog.ColumnDef{}
	for _, c := range s.Tables[0].Def.Columns {
		byName[c.Name] = c
	}

	if byName["id"].Default.Func != catalog.DefaultUUID || byName["id"].Format != "uuid" {
		t.Errorf("id: %+v", byName["id"])
	}
	if byName["email"].Format != "email" {
		t.Errorf("email: %+v", byName["email"])
	}
	if byName["status"].Default.Text != "open" {
		t.Errorf("status: %+v", byName["status"].Default)
	}
	if byName["attempts"].Default.Int64 != 3 {
		t.Errorf("attempts: %+v", byName["attempts"].Default)
	}
	if byName["ratio"].Default.Float64 != 0.5 {
		t.Errorf("ratio: %+v", byName["ratio"].Default)
	}
	if d := byName["done"].Default; d.Bool != false || d.Func != "" {
		t.Errorf("done: %+v", d)
	}
	if byName["at"].Default.Func != catalog.DefaultNowMS {
		t.Errorf("at: %+v", byName["at"].Default)
	}
}

func TestSchemaIDs(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: people
    columns:
      - { id: 1, name: id,        type: string, pk: true }
      - { id: 2, name: full_name, type: string }
`)
	tbl := s.Tables[0]
	if tbl.Def.ID != 1 {
		t.Errorf("table ID = %d", tbl.Def.ID)
	}
	if tbl.Def.Columns[1].ID != 2 {
		t.Errorf("column ID = %d", tbl.Def.Columns[1].ID)
	}
}

func TestCanonicalSchemaIncludesLogicalIDs(t *testing.T) {
	s := parse(t, `
tables:
  - id: 1
    name: zed
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 2, name: title, type: string }
  - id: 2
    name: alpha
    columns:
      - { id: 1, name: id, type: int64, pk: true }
`)

	canonical := s.Canonical()
	if len(canonical.Tables) != 2 || canonical.Tables[0].Name != "zed" || canonical.Tables[1].Name != "alpha" {
		t.Fatalf("canonical tables = %+v", canonical.Tables)
	}
	raw, err := canonical.CanonicalJSON()
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(raw), `"id":1`) || !strings.Contains(string(raw), `"id":2`) {
		t.Fatalf("canonical schema omitted logical IDs: %s", raw)
	}
}

// Structural errors come from the embedded JSON Schema; semantic errors
// (default/type mismatches, pk conflicts, duplicates) from the compiler.
func TestParseErrors(t *testing.T) {
	cases := []struct {
		name, src, wantErr string
	}{
		{"unknown type", `
tables:
  - id: 1
    name: t
    columns: [{ name: a, type: uuid, pk: true }]
`, "jsonschema"},
		{"unknown attribute", `
tables:
  - id: 1
    name: t
    columns: [{ name: a, type: string, wat: true }]
`, "jsonschema"},
		{"bad ref shape", `
tables:
  - id: 1
    name: t
    columns: [{ name: a, type: string, ref: users }]
`, "jsonschema"},
		{"bad identifier", `
tables:
  - id: 1
    name: T-1
    columns: [{ name: a, type: string }]
`, "jsonschema"},
		{"not yaml", "table users {}", "jsonschema"},
		{"duplicate table", `
tables:
  - id: 1
    name: t
    columns: [{ id: 1, name: a, type: string, pk: true }]
  - id: 2
    name: t
    columns: [{ id: 1, name: a, type: string, pk: true }]
`, "duplicate table"},
		{"typed default mismatch", `
tables:
  - id: 1
    name: t
    columns: [{ id: 1, name: a, type: int64, default: nope }]
`, "string default on int64"},
		{"missing table id", `
tables:
  - name: t
    columns: [{ id: 1, name: a, type: string, pk: true }]
`, "jsonschema"},
		{"duplicate table id", `
tables:
  - id: 1
    name: a
    columns: [{ id: 1, name: id, type: string, pk: true }]
  - id: 1
    name: b
    columns: [{ id: 1, name: id, type: string, pk: true }]
`, "share ID 1"},
		{"duplicate column id", `
tables:
  - id: 1
    name: t
    columns:
      - { id: 1, name: id, type: string, pk: true }
      - { id: 1, name: title, type: string }
`, "share ID 1"},
		{"rename hint removed", `
tables:
  - id: 1
    name: t
    renamed_from: old_t
    columns: [{ id: 1, name: id, type: string, pk: true }]
`, "jsonschema"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := schema.Parse("test.rad", []byte(tc.src))
			if err == nil {
				t.Fatalf("accepted, want error containing %q", tc.wantErr)
			}
			if tc.wantErr != "jsonschema" && !strings.Contains(err.Error(), tc.wantErr) {
				t.Fatalf("got %v, want error containing %q", err, tc.wantErr)
			}
			if !strings.Contains(err.Error(), "test.rad") {
				t.Errorf("error lacks filename: %v", err)
			}
		})
	}
}

// The demo application's schema is the reference document for the format;
// it must parse, and its shape sanity-checks the features it exercises.
func TestParseDemoSchema(t *testing.T) {
	src, err := os.ReadFile("../../../../examples/demo/rad.schema.yaml")
	if err != nil {
		t.Skipf("demo schema not available: %v", err)
	}
	s, err := schema.Parse("examples/demo/rad.schema.yaml", src)
	if err != nil {
		t.Fatal(err)
	}
	if len(s.Tables) != 9 {
		t.Fatalf("demo schema has %d tables, want 9", len(s.Tables))
	}

	tasks, ok := s.Table("tasks")
	if !ok {
		t.Fatal("tasks table missing")
	}
	if len(tasks.Def.ForeignKeys) != 4 { // board, assignee, creator, parent(self)
		t.Errorf("tasks has %d FKs, want 4: %+v", len(tasks.Def.ForeignKeys), tasks.Def.ForeignKeys)
	}
	members, _ := s.Table("team_members")
	if len(members.Def.PrimaryKey) != 2 {
		t.Errorf("team_members pk = %v, want composite", members.Def.PrimaryKey)
	}
	boards, _ := s.Table("boards")
	foundCompositeUnique := false
	for _, idx := range boards.Def.Indexes {
		if idx.Unique && len(idx.Columns) == 2 {
			foundCompositeUnique = true
		}
	}
	if !foundCompositeUnique {
		t.Error("boards lacks the composite unique constraint")
	}
}
