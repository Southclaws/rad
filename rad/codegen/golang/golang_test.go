package golang

import (
	"bytes"
	"go/format"
	"testing"

	"github.com/Southclaws/rad/rad/codegen"
)

// selfRefModel builds a one-table model that exercises every branch the
// generator cares about: a nullable column, a defaulted column, a unique
// index, and a self-referencing foreign key (so both a Forward and a Reverse
// relation are emitted against the same table).
func selfRefModel() *codegen.Model {
	t := &codegen.Table{Name: "categories", Model: "Category"}
	id := codegen.Col{Name: "id", Field: "ID", GoType: "string", IsPK: true}
	name := codegen.Col{Name: "name", Field: "Name", GoType: "string"}
	parent := codegen.Col{Name: "parent_id", Field: "ParentID", GoType: "string", Nullable: true}
	rank := codegen.Col{Name: "rank", Field: "Rank", GoType: "int64", HasDef: true}
	t.Cols = []codegen.Col{id, name, parent, rank}
	t.PK = []codegen.Col{id}
	t.Uniques = [][]codegen.Col{{name}}
	t.Forward = []codegen.Rel{{
		Field:  "Parent",
		As:     "parent",
		FKName: "categories_parent_id_fk",
		Target: t,
		Cols:   []codegen.Col{parent},
		Pairs:  [][2]string{{"id", "parent_id"}},
	}}
	t.Reverse = []codegen.Rel{{
		Field:  "Categories",
		As:     "categories",
		FKName: "categories_parent_id_fk",
		Target: t,
		Cols:   []codegen.Col{parent},
		Pairs:  [][2]string{{"parent_id", "id"}},
	}}
	return &codegen.Model{Pkg: "testclient", Tables: []*codegen.Table{t}}
}

func TestGenerate(t *testing.T) {
	files, err := generator{}.Generate(selfRefModel(), codegen.Options{
		Package: "testclient", SchemaVersion: 4, SchemaHash: "sha256:accepted",
		SchemaSource: []byte("tables: []\n"),
	})
	if err != nil {
		t.Fatalf("Generate: %v", err)
	}
	if len(files) != 1 {
		t.Fatalf("want 1 file, got %d", len(files))
	}
	f := files[0]
	if f.Path != codegen.GoClientFilename {
		t.Errorf("path = %q, want %s", f.Path, codegen.GoClientFilename)
	}
	if len(f.Content) == 0 {
		t.Fatal("empty content")
	}

	// The output claims to be gofmt-ed; prove it is.
	formatted, err := format.Source(f.Content)
	if err != nil {
		t.Fatalf("generated source does not format: %v", err)
	}
	if !bytes.Equal(formatted, f.Content) {
		t.Error("generated source is not idempotently gofmt-ed")
	}

	// Every LIR builder must be the lirwire form, never the old protocol one.
	forbidden := []string{
		"protocol.Eq",
		"protocol.Col(",
		"protocol.Node",
		"protocol.OrderTerm",
		"protocol.AggTerm",
		"protocol.Field",
		"protocol.Query",
	}
	for _, bad := range forbidden {
		if bytes.Contains(f.Content, []byte(bad)) {
			t.Errorf("output still contains %q — not ported to lirwire", bad)
		}
	}
	if !bytes.Contains(f.Content, []byte("lirwire.")) {
		t.Error("output has no lirwire.* references")
	}
	// protocol.Record is kept, so the protocol import must remain.
	if !bytes.Contains(f.Content, []byte("protocol.Record")) {
		t.Error("expected protocol.Record to be retained")
	}
	for _, metadata := range []string{
		"const SchemaVersion uint64 = 4",
		`const SchemaHash = "sha256:accepted"`,
		`const RawSchema = "tables: []\n"`,
		"rc.ExpectSchema(SchemaVersion, SchemaHash)",
	} {
		if !bytes.Contains(f.Content, []byte(metadata)) {
			t.Errorf("generated source is missing %q", metadata)
		}
	}
}
