package schema_test

import (
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

func TestCanonicalHashIgnoresYAMLFormattingAndKeyOrder(t *testing.T) {
	one := `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true }
`
	two := `
# The comment and presentation do not participate in schema identity.
tables:
  - columns:
      - type: string
        pk: true
        name: id
        id: 1
    name: users
    id: 1
`
	a := parseCanonical(t, one)
	b := parseCanonical(t, two)
	hashA, err := a.Hash()
	if err != nil {
		t.Fatal(err)
	}
	hashB, err := b.Hash()
	if err != nil {
		t.Fatal(err)
	}
	if hashA != hashB {
		t.Fatalf("semantic hashes differ: %s != %s", hashA, hashB)
	}
}

func TestRenderRoundTripsCanonicalSchema(t *testing.T) {
	source := `
tables:
  - id: 1
    name: users
    columns:
      - { id: 1, name: id, type: string, pk: true, default: uuid() }
      - { id: 2, name: email, type: string, nullable: true }
    indexes:
      - { name: users_email_lookup, columns: [email] }
`
	want := parseCanonical(t, source)
	rendered, err := schema.Render(want)
	if err != nil {
		t.Fatal(err)
	}
	got := parseCanonical(t, string(rendered))
	equal, err := want.Equal(got)
	if err != nil || !equal {
		t.Fatalf("rendered schema did not round trip: equal=%t err=%v\n%s", equal, err, rendered)
	}
}

func parseCanonical(t *testing.T, source string) catalog.Schema {
	t.Helper()
	parsed, err := schema.Parse("rad.schema.yaml", []byte(source))
	if err != nil {
		t.Fatal(err)
	}
	return parsed.Canonical()
}
