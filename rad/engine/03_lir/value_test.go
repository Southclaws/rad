package lir

import (
	"math"
	"testing"
)

// TestFloat64FoldsNegativeZero: the two zeros compare equal, so nothing derived
// from a value (a key encoding, a group key, a row identity) may tell them apart
func TestFloat64FoldsNegativeZero(t *testing.T) {
	negZero, zero := Float64(math.Copysign(0, -1)), Float64(0)
	if math.Signbit(negZero.Float64) {
		t.Fatalf("Float64(-0.0) kept the sign bit: %v", negZero.Float64)
	}
	if !negZero.Equal(zero) {
		t.Fatal("the two zeros must be equal")
	}
	if a, b := negZero.AppendIdentity(nil), zero.AppendIdentity(nil); string(a) != string(b) {
		t.Fatalf("row identity split the zeros: %q vs %q", a, b)
	}
}