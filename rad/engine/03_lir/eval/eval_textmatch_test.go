package eval

// text_match evaluation, pinned enumeratively: a NULL value is UNKNOWN,
// literal comparison follows the compiled mode, and a boolean result flows
// through both Eval (as a bool Value) and EvalPred (as a TriBool). The
// reference interpreter shares this evaluator, so these tests make that
// sharing safe.

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

func pat(t *testing.T, parts ...lir.TextMatchPart) bound.TextPattern {
	t.Helper()
	p, err := bound.CompileTextPattern(parts, lir.TextComparisonExact)
	if err != nil {
		t.Fatal(err)
	}
	return p
}

func TestTextMatchUnicodeSimpleFold(t *testing.T) {
	p, err := bound.CompileTextPattern(
		[]lir.TextMatchPart{lir.LiteralPart{Value: "foo"}},
		lir.TextComparisonUnicodeSimpleFold,
	)
	if err != nil {
		t.Fatal(err)
	}
	m := bound.NewTextMatch(lit(lir.Text("FOO")), p)
	v, err := Eval(m, nil)
	if err != nil {
		t.Fatal(err)
	}
	if v.Null || !v.Bool {
		t.Fatalf(`"FOO" must match literal "foo" under Unicode simple folding, got %v`, v)
	}
}

func TestTextMatchNullValueIsUnknown(t *testing.T) {
	m := bound.NewTextMatch(lit(lir.Null(model.TypeText)), pat(t, lir.AnyManyPart{}))

	v, err := Eval(m, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !v.Null {
		t.Fatalf("NULL value should evaluate to a NULL bool, got %v", v)
	}

	tri, err := EvalPred(m, nil)
	if err != nil {
		t.Fatal(err)
	}
	if tri != lir.TriUnknown {
		t.Fatalf("NULL value as a predicate = %v, want UNKNOWN", tri)
	}
}

func TestTextMatchByteExact(t *testing.T) {
	m := bound.NewTextMatch(lit(lir.Text("Foo")), pat(t, lir.LiteralPart{Value: "foo"}))
	v, err := Eval(m, nil)
	if err != nil {
		t.Fatal(err)
	}
	if v.Null || v.Bool {
		t.Fatalf(`"Foo" must not match literal "foo" byte-exact, got %v`, v)
	}
}

func TestTextMatchThroughPred(t *testing.T) {
	prefix := pat(t, lir.LiteralPart{Value: "foo"}, lir.AnyManyPart{})

	hit := bound.NewTextMatch(lit(lir.Text("foobar")), prefix)
	if tri, err := EvalPred(hit, nil); err != nil || tri != lir.TriTrue {
		t.Fatalf("`foobar` prefix `foo` = %v (err %v), want TRUE", tri, err)
	}

	miss := bound.NewTextMatch(lit(lir.Text("xfoo")), prefix)
	if tri, err := EvalPred(miss, nil); err != nil || tri != lir.TriFalse {
		t.Fatalf("`xfoo` prefix `foo` = %v (err %v), want FALSE", tri, err)
	}
}
