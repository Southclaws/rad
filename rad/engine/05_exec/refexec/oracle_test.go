package refexec

// Focused tests for refexec's own semantic components. The interpreter is one
// oracle among several and no longer trivially small, so its low-level pieces —
// the independent canonical row identity and the recursive-projection
// invariants — are pinned directly here rather than only through the engine
// differential. Testing the oracle is not circular: these fix its behaviour
// from hand-derived cases, and the composed interpreter then checks the engine.

import (
	"math"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

// TestOracleRowSetIdentity pins full-row identity: NULL == NULL, identity spans
// every field, and a value distinguishes a row from one where that field is
// NULL.
func TestOracleRowSetIdentity(t *testing.T) {
	s := newOracleRowSet([]lir.Field{{Slot: 0}, {Slot: 1}})
	row := func(a, b lir.Datum) lireval.Env { return lireval.Env{0: a, 1: b} }
	txt := func(x string) lir.Datum { return lir.ScalarDatum(lir.Text(x)) }
	i64 := func(x int64) lir.Datum { return lir.ScalarDatum(lir.Int64(x)) }
	null := lir.NullDatum()

	for _, c := range []struct {
		name string
		row  lireval.Env
		want bool // add reports "newly seen"?
	}{
		{"first row", row(txt("x"), i64(1)), true},
		{"exact duplicate", row(txt("x"), i64(1)), false},
		{"differ in one column", row(txt("x"), i64(2)), true},
		{"first all-null", row(null, null), true},
		{"null == null duplicate", row(null, null), false},
		{"value vs null in a column", row(txt("x"), null), true},
		{"duplicate null-tailed", row(txt("x"), null), false},
	} {
		if got := s.add(c.row); got != c.want {
			t.Errorf("%s: add=%v want %v", c.name, got, c.want)
		}
	}
}

// TestOracleRowSetKinds: different scalar kinds never collide even when they
// render alike, because identity is typed.
func TestOracleRowSetKinds(t *testing.T) {
	s := newOracleRowSet([]lir.Field{{Slot: 0}})
	one := func(d lir.Datum) lireval.Env { return lireval.Env{0: d} }
	for i, d := range []lir.Datum{
		lir.ScalarDatum(lir.Text("1")),
		lir.ScalarDatum(lir.Int64(1)),
		lir.ScalarDatum(lir.Float64(1)),
		lir.ScalarDatum(lir.Bool(true)),
	} {
		if !s.add(one(d)) {
			t.Errorf("row %d should be a distinct kind, not a collision", i)
		}
	}
}

// TestOracleRowSetFloatBits pins refexec's divergence: floats identify by bit
// pattern, so the raw value is built here rather than through lir.Float64
func TestOracleRowSetFloatBits(t *testing.T) {
	s := newOracleRowSet([]lir.Field{{Slot: 0}})
	f := func(v float64) lireval.Env {
		return lireval.Env{0: lir.ScalarDatum(lir.Value{Type: model.TypeFloat64, Float64: v})}
	}
	if !s.add(f(0)) {
		t.Fatal("first +0.0 should be new")
	}
	if s.add(f(0)) {
		t.Fatal("+0.0 duplicate should not be new")
	}
	if !s.add(f(math.Copysign(0, -1))) {
		t.Fatal("-0.0 should be distinct from +0.0 under bit identity")
	}
}

// TestMakeProjectionByName: the canonical projection maps source slots to
// output slots by name, independent of source field order.
func TestMakeProjectionByName(t *testing.T) {
	canon := []lir.Field{{Name: "id", Slot: 10}, {Name: "depth", Slot: 11}}
	source := []lir.Field{{Name: "depth", Slot: 3}, {Name: "id", Slot: 7}}
	pairs, err := makeProjection(canon, source)
	if err != nil {
		t.Fatal(err)
	}
	row := lireval.Env{7: lir.ScalarDatum(lir.Text("a")), 3: lir.ScalarDatum(lir.Int64(2))}
	out, err := project(row, pairs)
	if err != nil {
		t.Fatal(err)
	}
	if out[10].Scalar.Text != "a" || out[11].Scalar.Int64 != 2 {
		t.Fatalf("projection mapped wrong slots: %+v", out)
	}
}

// TestProjectionInvariantsErrorLoudly: a canonical field absent from the source,
// or a source slot missing from a row, is a broken bound-plan invariant and
// must error — never silently read slot zero or drop a datum (which could let a
// "missing" cell masquerade as NULL).
func TestProjectionInvariantsErrorLoudly(t *testing.T) {
	source := []lir.Field{{Name: "id", Slot: 7}}
	if _, err := makeProjection([]lir.Field{{Name: "ghost", Slot: 0}}, source); err == nil {
		t.Error("makeProjection should error on a canonical field missing from the source")
	}
	pairs, err := makeProjection([]lir.Field{{Name: "id", Slot: 10}}, source)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := project(lireval.Env{}, pairs); err == nil {
		t.Error("project should error on a row missing an expected source slot")
	}
}
