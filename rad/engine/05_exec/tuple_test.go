package exec

import (
	"bytes"
	"sort"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestTupleRoundTrip(t *testing.T) {
	tuples := [][]lir.Value{
		{lir.Int64(0)},
		{lir.Int64(-9223372036854775808), lir.Int64(9223372036854775807)},
		{lir.Float64(-1.5), lir.Float64(0), lir.Float64(3.14159)},
		{lir.Text(""), lir.Text("hello"), lir.Text("a\x00b\x00\x00c")},
		{lir.Null(catalog.TypeText)},
		{lir.Int64(42), lir.Text("x"), lir.Float64(2.5), {Null: true}},
	}
	for _, tup := range tuples {
		enc, err := EncodeTuple(tup)
		if err != nil {
			t.Fatal(err)
		}
		dec, err := DecodeTuple(enc)
		if err != nil {
			t.Fatalf("decode %x: %v", enc, err)
		}
		if len(dec) != len(tup) {
			t.Fatalf("got %d values, want %d", len(dec), len(tup))
		}
		for i := range tup {
			if tup[i].Null {
				if !dec[i].Null {
					t.Fatalf("value %d: expected null, got %v", i, dec[i])
				}
				continue
			}
			if !dec[i].Equal(tup[i]) {
				t.Fatalf("value %d: got %v, want %v", i, dec[i], tup[i])
			}
		}
	}
}

func TestTupleOrdering(t *testing.T) {
	tuples := [][]lir.Value{
		{lir.Null(catalog.TypeInt64), lir.Text("z")},
		{lir.Int64(1), lir.Text("a")},
		{lir.Int64(1), lir.Text("b")},
		{lir.Int64(2), lir.Text("a")},
		{lir.Int64(10), lir.Text("a")},
		{lir.Text("x"), lir.Int64(5)},
	}
	enc := make([][]byte, len(tuples))
	for i, tup := range tuples {
		var err error
		enc[i], err = EncodeTuple(tup)
		if err != nil {
			t.Fatal(err)
		}
	}
	if !sort.SliceIsSorted(enc, func(i, j int) bool { return bytes.Compare(enc[i], enc[j]) < 0 }) {
		t.Errorf("encoded tuples are not in expected order")
	}
}

func TestEncodeRejectsNaN(t *testing.T) {
	zero := 0.0
	nan := lir.Value{Type: catalog.TypeFloat64, Float64: zero / zero}
	if _, err := EncodeTuple([]lir.Value{nan}); err == nil {
		t.Error("expected error encoding NaN")
	}
}
