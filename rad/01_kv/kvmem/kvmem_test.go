package kvmem

import (
	"bytes"
	"context"
	"testing"

	"rad/rad/01_kv"
	"rad/rad/01_kv/kvtest"
)

func TestTransactionConformance(t *testing.T) {
	kvtest.Run(t, func(t *testing.T) kv.TransactionalKV { return New() })
}

func TestPutGetDelete(t *testing.T) {
	ctx := context.Background()
	s := New()

	if _, ok, _ := s.Get(ctx, []byte("k")); ok {
		t.Fatal("expected missing key")
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
	s := New()
	for _, k := range []string{"b", "a", "d", "c", "e"} {
		if err := s.Put(ctx, []byte(k), []byte("v"+k)); err != nil {
			t.Fatal(err)
		}
	}

	it, err := s.Scan(ctx, []byte("b"), []byte("e"))
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
	want := []string{"b", "c", "d"}
	if len(got) != len(want) {
		t.Fatalf("got %v, want %v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("got %v, want %v", got, want)
		}
	}
}

func TestScanUnbounded(t *testing.T) {
	ctx := context.Background()
	s := New()
	for _, k := range []string{"x", "y", "z"} {
		_ = s.Put(ctx, []byte(k), nil)
	}
	it, err := s.Scan(ctx, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	n := 0
	for it.Next() {
		n++
	}
	if n != 3 {
		t.Fatalf("got %d keys, want 3", n)
	}
}
