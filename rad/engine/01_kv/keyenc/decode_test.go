package keyenc

import (
	"math/rand"
	"testing"
)

func TestPrimitiveRoundTrip(t *testing.T) {
	ints := []int64{-9223372036854775808, -1, 0, 1, 42, 9223372036854775807}
	for _, v := range ints {
		got, n, err := DecodeInt64(EncodeInt64(v))
		if err != nil || got != v || n != 9 {
			t.Fatalf("int64 %d: got %d n=%d err=%v", v, got, n, err)
		}
	}

	floats := []float64{-1e308, -1.5, 0, 3.14159, 1e308}
	for _, v := range floats {
		got, _, err := DecodeFloat64(EncodeFloat64(v))
		if err != nil || got != v {
			t.Fatalf("float64 %g: got %g err=%v", v, got, err)
		}
	}

	strs := []string{"", "hello", "a\x00b\x00\x00c", "\xff\xfe"}
	for _, v := range strs {
		enc := EncodeString(v)
		got, n, err := DecodeString(enc)
		if err != nil || got != v || n != len(enc) {
			t.Fatalf("string %q: got %q n=%d err=%v", v, got, n, err)
		}
	}

	if n, err := DecodeNull(EncodeNull()); err != nil || n != 1 {
		t.Fatalf("null: n=%d err=%v", n, err)
	}

	for _, b := range []bool{false, true} {
		got, n, err := DecodeBool(EncodeBool(b))
		if err != nil || got != b || n != 2 {
			t.Fatalf("bool %v: got %v n=%d err=%v", b, got, n, err)
		}
	}
}

func TestRandomStringRoundTrip(t *testing.T) {
	r := rand.New(rand.NewSource(3))
	for range 500 {
		b := make([]byte, r.Intn(24))
		for i := range b {
			b[i] = byte(r.Intn(256)) // includes NUL and 0xFF edge cases
		}
		s := string(b)
		got, _, err := DecodeString(EncodeString(s))
		if err != nil || got != s {
			t.Fatalf("%q: got %q err=%v", s, got, err)
		}
	}
}

// Self-delimiting: a decode consumes exactly its own encoding even with
// trailing data, so concatenated values split unambiguously.
func TestDecodeWithTrailingData(t *testing.T) {
	buf := append(EncodeString("ab"), EncodeInt64(7)...)

	s, n, err := DecodeString(buf)
	if err != nil || s != "ab" {
		t.Fatalf("got %q err=%v", s, err)
	}
	i, _, err := DecodeInt64(buf[n:])
	if err != nil || i != 7 {
		t.Fatalf("got %d err=%v", i, err)
	}
}

func TestPeek(t *testing.T) {
	cases := map[byte][]byte{
		TagNull:    EncodeNull(),
		TagBool:    EncodeBool(true),
		TagInt64:   EncodeInt64(1),
		TagFloat64: EncodeFloat64(1),
		TagText:    EncodeString("x"),
	}
	for want, buf := range cases {
		got, err := Peek(buf)
		if err != nil || got != want {
			t.Fatalf("Peek(%x) = 0x%02x err=%v, want 0x%02x", buf, got, err, want)
		}
	}
	if _, err := Peek(nil); err == nil {
		t.Error("Peek(nil) succeeded")
	}
	if _, err := Peek([]byte{0x99}); err == nil {
		t.Error("Peek(unknown tag) succeeded")
	}
}

func TestDecodeErrors(t *testing.T) {
	if _, _, err := DecodeInt64([]byte{TagInt64, 0x01}); err == nil {
		t.Error("truncated int64 decoded")
	}
	if _, _, err := DecodeFloat64([]byte{TagFloat64, 0x00}); err == nil {
		t.Error("truncated float64 decoded")
	}
	if _, _, err := DecodeString([]byte{TagText, 'a'}); err == nil {
		t.Error("unterminated string decoded")
	}
	if _, _, err := DecodeString([]byte{TagText, 0x00}); err == nil {
		t.Error("truncated escape decoded")
	}
	if _, _, err := DecodeInt64(EncodeString("x")); err == nil {
		t.Error("wrong-tag decode succeeded")
	}
}
