package exec

import (
	"bytes"
	"sort"
	"testing"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

func TestTupleRoundTrip(t *testing.T) {
	tuples := [][]qir.Value{
		{qir.Int64(0)},
		{qir.Int64(-9223372036854775808), qir.Int64(9223372036854775807)},
		{qir.Float64(-1.5), qir.Float64(0), qir.Float64(3.14159)},
		{qir.Text(""), qir.Text("hello"), qir.Text("a\x00b\x00\x00c")},
		{qir.Null(catalog.TypeText)},
		{qir.Int64(42), qir.Text("x"), qir.Float64(2.5), {Null: true}},
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
	tuples := [][]qir.Value{
		{qir.Null(catalog.TypeInt64), qir.Text("z")},
		{qir.Int64(1), qir.Text("a")},
		{qir.Int64(1), qir.Text("b")},
		{qir.Int64(2), qir.Text("a")},
		{qir.Int64(10), qir.Text("a")},
		{qir.Text("x"), qir.Int64(5)},
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
	nan := qir.Value{Type: catalog.TypeFloat64, Float64: zero / zero}
	if _, err := EncodeTuple([]qir.Value{nan}); err == nil {
		t.Error("expected error encoding NaN")
	}
}
