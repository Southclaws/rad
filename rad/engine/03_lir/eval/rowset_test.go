package eval

import (
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// TestCanonicalRowSet pins the shared full-row identity the distinct operator
// and recursive admit-new accumulation both depend on: NULL == NULL, identity
// spans every field, and a value in one column distinguishes a row from one
// where that column is NULL.
func TestCanonicalRowSet(t *testing.T) {
	s := NewCanonicalRowSet([]lir.Field{{Slot: 0}, {Slot: 1}})

	row := func(a, b lir.Datum) Env { return Env{0: a, 1: b} }
	txt := func(x string) lir.Datum { return lir.ScalarDatum(lir.Text(x)) }
	i64 := func(x int64) lir.Datum { return lir.ScalarDatum(lir.Int64(x)) }
	null := lir.NullDatum()

	cases := []struct {
		name string
		row  Env
		want bool // Add reports "newly seen"?
	}{
		{"first row is new", row(txt("x"), i64(1)), true},
		{"exact duplicate is not new", row(txt("x"), i64(1)), false},
		{"differ in one column", row(txt("x"), i64(2)), true},
		{"first all-null row is new", row(null, null), true},
		{"null == null: duplicate all-null not new", row(null, null), false},
		{"a value distinguishes from null in that column", row(txt("x"), null), true},
		{"duplicate of the null-tailed row not new", row(txt("x"), null), false},
	}
	for _, c := range cases {
		if got := s.Add(c.row); got != c.want {
			t.Errorf("%s: Add=%v want %v", c.name, got, c.want)
		}
	}

	if !s.Contains(row(txt("x"), i64(1))) {
		t.Error("Contains should report an added row")
	}
	if s.Contains(row(txt("z"), i64(9))) {
		t.Error("Contains should reject a never-added row")
	}
}

// TestCanonicalRowSetKinds: different scalar kinds never collide even when they
// render to the same text, because the key is type-tagged.
func TestCanonicalRowSetKinds(t *testing.T) {
	s := NewCanonicalRowSet([]lir.Field{{Slot: 0}})
	one := func(d lir.Datum) Env { return Env{0: d} }
	for i, d := range []lir.Datum{
		lir.ScalarDatum(lir.Text("1")),
		lir.ScalarDatum(lir.Int64(1)),
		lir.ScalarDatum(lir.Float64(1)),
		lir.ScalarDatum(lir.Bool(true)),
	} {
		if !s.Add(one(d)) {
			t.Errorf("row %d (kind %v) should be distinct, not a collision", i, d.Scalar.Type)
		}
	}
}
