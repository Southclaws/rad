package typescript

import (
	"bytes"
	"testing"

	"github.com/Southclaws/rad/rad/codegen"
)

func TestGenerateEmbedsAcceptedIdentityAndChecksCompatibility(t *testing.T) {
	files, err := generator{}.Generate(&codegen.Model{Pkg: "db"}, codegen.Options{
		Package: "db", SchemaVersion: 7, SchemaHash: "sha256:accepted",
		SchemaSource: []byte("tables: []\n"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(files) != 1 {
		t.Fatalf("files = %d", len(files))
	}
	if files[0].Path != codegen.TypeScriptClientFilename {
		t.Errorf("path = %q, want %s", files[0].Path, codegen.TypeScriptClientFilename)
	}
	source := files[0].Content
	for _, expected := range [][]byte{
		[]byte("export const schemaVersion = 7;"),
		[]byte(`export const schemaHash = "sha256:accepted";`),
		[]byte("export const rawSchema = `tables: []"),
		[]byte(`this.rawReq<void>("/schema/compatibility"`),
	} {
		if !bytes.Contains(source, expected) {
			t.Errorf("generated source is missing %q", expected)
		}
	}
	if bytes.Contains(source, []byte("async migrate")) || bytes.Contains(source, []byte("/migrate")) {
		t.Fatal("generated client still exposes schema migration")
	}
}
