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
