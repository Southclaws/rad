package lirwire

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestTextMatchComparisonBuilders(t *testing.T) {
	exact, err := json.Marshal(TextMatch(Col("t", "name"), LiteralPart("foo")))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(exact), `"comparison"`) {
		t.Fatalf("default exact comparison should be omitted, got %s", exact)
	}

	explicitExact, err := json.Marshal(TextMatchWithComparison(
		Col("t", "name"), TextComparisonExact, LiteralPart("foo"),
	))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(explicitExact), `"comparison"`) {
		t.Fatalf("explicit exact comparison should use the default wire form, got %s", explicitExact)
	}

	folded, err := json.Marshal(TextMatchWithComparison(
		Col("t", "name"), TextComparisonUnicodeSimpleFold, LiteralPart("foo"),
	))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(folded), `"comparison":"unicode_simple_fold"`) {
		t.Fatalf("simple-fold comparison missing from wire form: %s", folded)
	}
}

func TestBytesBuildersUseCanonicalBase64(t *testing.T) {
	value, err := json.Marshal(Bytes([]byte{0, 0xff}))
	if err != nil {
		t.Fatal(err)
	}
	if got := string(value); got != `{"type":"bytes","value":"AP8="}` {
		t.Fatalf("unexpected bytes literal: %s", got)
	}

	cell, err := MakeCell(ScalarTypeBytes, [16]byte{0xff})
	if err != nil {
		t.Fatal(err)
	}
	if cell == nil || *cell != "/wAAAAAAAAAAAAAAAAAAAA==" {
		t.Fatalf("unexpected bytes cell: %v", cell)
	}

	expression, err := json.Marshal(LitOf([12]byte{1, 2, 3}))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(expression), `"type":"bytes","value":"AQIDAAAAAAAAAAAA"`) {
		t.Fatalf("array literal did not become bytes: %s", expression)
	}
}
