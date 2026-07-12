package kvslate

import (
	"bytes"
	"context"
	"fmt"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvtest"
)

func open(t *testing.T) *Store {
	t.Helper()
	s, err := Open(fmt.Sprintf("test-%s", t.Name()), "memory:///")
	if err != nil {
		t.Fatalf("open slatedb: %v", err)
	}
	t.Cleanup(func() {
		if err := s.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return s
}

func TestTransactionConformance(t *testing.T) {
	kvtest.Run(t, func(t *testing.T) kv.TransactionalKV { return open(t) })
}

func TestPutGetDelete(t *testing.T) {
	ctx := context.Background()
	s := open(t)

	if _, ok, err := s.Get(ctx, []byte("missing")); err != nil || ok {
		t.Fatalf("expected missing key, ok=%v err=%v", ok, err)
	}
	if err := s.Put(ctx, []byte("k"), []byte("v")); err != nil {
		t.Fatal(err)
	}
	v, ok, err := s.Get(ctx, []byte("k"))
	if err != nil || !ok || !bytes.Equal(v, []byte("v")) {
		t.Fatalf("got %q ok=%v err=%v", v, ok, err)
	}
	if err := s.Delete(ctx, []byte("k")); err != nil {
		t.Fatal(err)
	}
	if _, ok, _ := s.Get(ctx, []byte("k")); ok {
		t.Fatal("expected key deleted")
	}
}

func TestScanOrderingAndBounds(t *testing.T) {
	ctx := context.Background()
	s := open(t)

	// Insert out of order, including binary keys, to prove lexicographic
	// iteration order.
	keys := [][]byte{
		{0x02, 0xFF},
		{0x01},
		{0x02, 0x00},
		[]byte("zzz"),
		{0x02},
	}
	for _, k := range keys {
		if err := s.Put(ctx, k, k); err != nil {
			t.Fatal(err)
		}
	}

	it, err := s.Scan(ctx, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()

	var got [][]byte
	for it.Next() {
		got = append(got, bytes.Clone(it.Key()))
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	want := [][]byte{{0x01}, {0x02}, {0x02, 0x00}, {0x02, 0xFF}, []byte("zzz")}
	if len(got) != len(want) {
		t.Fatalf("got %d keys, want %d: %x", len(got), len(want), got)
	}
	for i := range want {
		if !bytes.Equal(got[i], want[i]) {
			t.Fatalf("key %d = %x, want %x", i, got[i], want[i])
		}
	}
}

func TestScanRangeBounds(t *testing.T) {
	ctx := context.Background()
	s := open(t)
	for _, k := range []string{"a", "b", "c", "d"} {
		if err := s.Put(ctx, []byte(k), nil); err != nil {
			t.Fatal(err)
		}
	}

	it, err := s.Scan(ctx, []byte("b"), []byte("d"))
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()

	var got []string
	for it.Next() {
		got = append(got, string(it.Key()))
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	if len(got) != 2 || got[0] != "b" || got[1] != "c" {
		t.Fatalf("got %v, want [b c] (start inclusive, end exclusive)", got)
	}
}

func TestPrefixScanWithPrefixEnd(t *testing.T) {
	ctx := context.Background()
	s := open(t)

	prefix := []byte("/rad/data/t1/primary/")
	other := []byte("/rad/data/t2/primary/")
	for i := range 5 {
		k := append(bytes.Clone(prefix), keyenc.EncodeInt64(int64(i))...)
		if err := s.Put(ctx, k, []byte{byte(i)}); err != nil {
			t.Fatal(err)
		}
	}
	if err := s.Put(ctx, other, nil); err != nil {
		t.Fatal(err)
	}

	it, err := s.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()

	n := 0
	prev := []byte(nil)
	for it.Next() {
		if !bytes.HasPrefix(it.Key(), prefix) {
			t.Fatalf("key %x escapes prefix", it.Key())
		}
		if prev != nil && bytes.Compare(prev, it.Key()) >= 0 {
			t.Fatalf("keys out of order: %x then %x", prev, it.Key())
		}
		prev = bytes.Clone(it.Key())
		n++
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	if n != 5 {
		t.Fatalf("got %d keys under prefix, want 5", n)
	}
}
