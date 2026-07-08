package keyenc

import (
	"bytes"
	"math/rand"
	"testing"
)

func TestEncodeInt64Ordering(t *testing.T) {
	vals := []int64{-9223372036854775808, -1000000, -2, -1, 0, 1, 2, 42, 1000000, 9223372036854775807}
	for i := 1; i < len(vals); i++ {
		a, b := EncodeInt64(vals[i-1]), EncodeInt64(vals[i])
		if bytes.Compare(a, b) >= 0 {
			t.Errorf("EncodeInt64(%d) >= EncodeInt64(%d)", vals[i-1], vals[i])
		}
	}

	r := rand.New(rand.NewSource(1))
	for range 1000 {
		x, y := r.Int63()-r.Int63(), r.Int63()-r.Int63()
		cmp := bytes.Compare(EncodeInt64(x), EncodeInt64(y))
		switch {
		case x < y && cmp >= 0, x > y && cmp <= 0, x == y && cmp != 0:
			t.Fatalf("ordering mismatch for %d vs %d: cmp=%d", x, y, cmp)
		}
	}
}

func TestEncodeFloat64Ordering(t *testing.T) {
	vals := []float64{-1e308, -100.5, -1.0, -0.001, 0, 0.001, 1.0, 2.5, 100.5, 1e308}
	for i := 1; i < len(vals); i++ {
		a, b := EncodeFloat64(vals[i-1]), EncodeFloat64(vals[i])
		if bytes.Compare(a, b) >= 0 {
			t.Errorf("EncodeFloat64(%g) >= EncodeFloat64(%g)", vals[i-1], vals[i])
		}
	}

	r := rand.New(rand.NewSource(2))
	for range 1000 {
		x, y := (r.Float64()-0.5)*1e9, (r.Float64()-0.5)*1e9
		cmp := bytes.Compare(EncodeFloat64(x), EncodeFloat64(y))
		switch {
		case x < y && cmp >= 0, x > y && cmp <= 0, x == y && cmp != 0:
			t.Fatalf("ordering mismatch for %g vs %g: cmp=%d", x, y, cmp)
		}
	}
}

func TestEncodeStringOrdering(t *testing.T) {
	vals := []string{"", "a", "app", "apple", "b", "ba\x00", "ba\x00\x00", "ba\x01", "bb"}
	for i := 1; i < len(vals); i++ {
		a, b := EncodeString(vals[i-1]), EncodeString(vals[i])
		if bytes.Compare(a, b) >= 0 {
			t.Errorf("EncodeString(%q) >= EncodeString(%q)", vals[i-1], vals[i])
		}
	}
}

// A string containing the terminator sequence must not make one encoded key a
// prefix collision with another ("a" + "\x00\x01b" vs "a\x00\x01" + "b").
func TestEncodeStringEmbeddedNulSafety(t *testing.T) {
	a := EncodeString("a\x00")
	b := EncodeString("a")
	if bytes.HasPrefix(a, b) {
		t.Errorf("encoding of %q is a prefix of encoding of %q", "a", "a\x00")
	}
}

// Tag ordering: null < int64 < float64 < text for any values.
func TestCrossTypeTagOrdering(t *testing.T) {
	seq := [][]byte{
		EncodeNull(),
		EncodeBool(false),
		EncodeBool(true),
		EncodeInt64(9223372036854775807),
		EncodeFloat64(1e308),
		EncodeString(""),
	}
	for i := 1; i < len(seq); i++ {
		if bytes.Compare(seq[i-1], seq[i]) >= 0 {
			t.Errorf("encoding %d does not sort before encoding %d", i-1, i)
		}
	}
}

func TestPrefixEnd(t *testing.T) {
	cases := []struct {
		in   []byte
		want []byte
	}{
		{[]byte{0x01}, []byte{0x02}},
		{[]byte{0x01, 0xFF}, []byte{0x02}},
		{[]byte{0xAB, 0xCD}, []byte{0xAB, 0xCE}},
		{[]byte{0xFF, 0xFF}, nil},
		{nil, nil},
	}
	for _, c := range cases {
		got := PrefixEnd(c.in)
		if !bytes.Equal(got, c.want) {
			t.Errorf("PrefixEnd(%x) = %x, want %x", c.in, got, c.want)
		}
	}

	// Every key starting with the prefix must fall in [prefix, PrefixEnd).
	prefix := []byte{0x03, 0xFF}
	end := PrefixEnd(prefix)
	for _, suffix := range [][]byte{{}, {0x00}, {0xFF}, {0xFF, 0xFF}} {
		key := append(bytes.Clone(prefix), suffix...)
		if bytes.Compare(key, prefix) < 0 || bytes.Compare(key, end) >= 0 {
			t.Errorf("key %x not in [%x, %x)", key, prefix, end)
		}
	}
}
