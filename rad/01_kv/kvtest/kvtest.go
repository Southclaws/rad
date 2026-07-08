// Package kvtest is a conformance suite for kv.TransactionalKV
// implementations — the executable definition of RAD's transaction
// semantics, run by the SlateDB adapter.
package kvtest

import (
	"bytes"
	"context"
	"errors"
	"testing"

	"rad/rad/01_kv"
)

// Run executes the transaction conformance suite. open must return a fresh,
// empty store for each call.
func Run(t *testing.T, open func(t *testing.T) kv.TransactionalKV) {
	t.Run("CommitVisibility", func(t *testing.T) { testCommitVisibility(t, open(t)) })
	t.Run("Rollback", func(t *testing.T) { testRollback(t, open(t)) })
	t.Run("SnapshotReads", func(t *testing.T) { testSnapshotReads(t, open(t)) })
	t.Run("ScanSeesOwnWrites", func(t *testing.T) { testScanSeesOwnWrites(t, open(t)) })
	t.Run("WriteWriteConflict", func(t *testing.T) { testWriteWriteConflict(t, open(t)) })
	t.Run("ReadKeyConflict", func(t *testing.T) { testReadKeyConflict(t, open(t)) })
	t.Run("PhantomRangeConflict", func(t *testing.T) { testPhantomRangeConflict(t, open(t)) })
	t.Run("WriteSkewAllowedUnderSnapshot", func(t *testing.T) { testWriteSkewAllowedUnderSnapshot(t, open(t)) })
	t.Run("WriteSkewBlockedUnderSerializable", func(t *testing.T) { testWriteSkewBlockedUnderSerializable(t, open(t)) })
}

func get(t *testing.T, s kv.KV, key string) ([]byte, bool) {
	t.Helper()
	v, ok, err := s.Get(context.Background(), []byte(key))
	if err != nil {
		t.Fatalf("get %q: %v", key, err)
	}
	return v, ok
}

func put(t *testing.T, s kv.KV, key, value string) {
	t.Helper()
	if err := s.Put(context.Background(), []byte(key), []byte(value)); err != nil {
		t.Fatalf("put %q: %v", key, err)
	}
}

func begin(t *testing.T, s kv.TransactionalKV, level kv.IsolationLevel) kv.Txn {
	t.Helper()
	txn, err := s.Begin(context.Background(), level)
	if err != nil {
		t.Fatalf("begin: %v", err)
	}
	return txn
}

func testCommitVisibility(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	txn := begin(t, s, kv.SerializableSnapshot)
	defer txn.Rollback()

	put(t, txn, "k", "v")
	if _, ok := get(t, s, "k"); ok {
		t.Fatal("uncommitted write visible outside the transaction")
	}
	if v, ok := get(t, txn, "k"); !ok || !bytes.Equal(v, []byte("v")) {
		t.Fatalf("transaction does not read its own write: %q ok=%v", v, ok)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatalf("commit: %v", err)
	}
	if v, ok := get(t, s, "k"); !ok || !bytes.Equal(v, []byte("v")) {
		t.Fatalf("committed write not visible: %q ok=%v", v, ok)
	}
}

func testRollback(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	txn := begin(t, s, kv.SerializableSnapshot)
	put(t, txn, "k", "v")
	if err := txn.Rollback(); err != nil {
		t.Fatalf("rollback: %v", err)
	}
	if _, ok := get(t, s, "k"); ok {
		t.Fatal("rolled-back write visible")
	}
	if err := txn.Rollback(); err != nil {
		t.Fatalf("second rollback should be a no-op, got %v", err)
	}

	txn = begin(t, s, kv.SerializableSnapshot)
	put(t, txn, "k2", "v")
	if err := txn.Commit(ctx); err != nil {
		t.Fatalf("commit: %v", err)
	}
	if err := txn.Rollback(); err != nil {
		t.Fatalf("rollback after commit should be a no-op, got %v", err)
	}
	if _, ok := get(t, s, "k2"); !ok {
		t.Fatal("rollback after commit must not undo the commit")
	}
}

func testSnapshotReads(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	put(t, s, "k", "v1")
	put(t, s, "r/a", "1")

	txn := begin(t, s, kv.Snapshot)
	defer txn.Rollback()

	// Concurrent direct writes after Begin are invisible to the snapshot.
	put(t, s, "k", "v2")
	put(t, s, "r/b", "2")

	if v, _ := get(t, txn, "k"); !bytes.Equal(v, []byte("v1")) {
		t.Fatalf("snapshot read got %q, want pre-Begin value v1", v)
	}
	it, err := txn.Scan(ctx, []byte("r/"), []byte("r0"))
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	var keys []string
	for it.Next() {
		keys = append(keys, string(it.Key()))
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	if len(keys) != 1 || keys[0] != "r/a" {
		t.Fatalf("snapshot scan saw %v, want [r/a]", keys)
	}
}

func testScanSeesOwnWrites(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	put(t, s, "r/a", "1")
	put(t, s, "r/b", "2")

	txn := begin(t, s, kv.SerializableSnapshot)
	defer txn.Rollback()
	put(t, txn, "r/c", "3")
	if err := txn.Delete(ctx, []byte("r/a")); err != nil {
		t.Fatal(err)
	}

	it, err := txn.Scan(ctx, []byte("r/"), []byte("r0"))
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	var keys []string
	for it.Next() {
		keys = append(keys, string(it.Key()))
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	if len(keys) != 2 || keys[0] != "r/b" || keys[1] != "r/c" {
		t.Fatalf("scan saw %v, want [r/b r/c] (own write added, own delete hidden)", keys)
	}
}

func testWriteWriteConflict(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	t1 := begin(t, s, kv.Snapshot)
	defer t1.Rollback()
	t2 := begin(t, s, kv.Snapshot)
	defer t2.Rollback()

	put(t, t1, "k", "t1")
	put(t, t2, "k", "t2")
	if err := t1.Commit(ctx); err != nil {
		t.Fatalf("first commit: %v", err)
	}
	err := t2.Commit(ctx)
	if !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("second commit: got %v, want kv.ErrConflict", err)
	}
	if v, _ := get(t, s, "k"); !bytes.Equal(v, []byte("t1")) {
		t.Fatalf("store has %q, want winner t1", v)
	}
}

func testReadKeyConflict(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	t1 := begin(t, s, kv.SerializableSnapshot)
	defer t1.Rollback()

	// t1 reads a key that does not exist yet; its commit writes elsewhere.
	if _, ok := get(t, t1, "watched"); ok {
		t.Fatal("expected missing key")
	}
	put(t, t1, "elsewhere", "x")

	// A concurrent transaction creates the watched key.
	t2 := begin(t, s, kv.SerializableSnapshot)
	defer t2.Rollback()
	put(t, t2, "watched", "y")
	if err := t2.Commit(ctx); err != nil {
		t.Fatalf("t2 commit: %v", err)
	}

	err := t1.Commit(ctx)
	if !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("t1 commit: got %v, want kv.ErrConflict (read key was created concurrently)", err)
	}
}

func testPhantomRangeConflict(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	t1 := begin(t, s, kv.SerializableSnapshot)
	defer t1.Rollback()

	// t1 scans an empty range (a unique-index existence check).
	it, err := t1.Scan(ctx, []byte("idx/"), []byte("idx0"))
	if err != nil {
		t.Fatal(err)
	}
	if it.Next() {
		t.Fatal("range should be empty")
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	if err := it.Close(); err != nil {
		t.Fatal(err)
	}
	put(t, t1, "idx/alice", "pk1")

	// t2 inserts a different key into the same range and wins the race.
	t2 := begin(t, s, kv.SerializableSnapshot)
	defer t2.Rollback()
	put(t, t2, "idx/alice2", "pk2")
	if err := t2.Commit(ctx); err != nil {
		t.Fatalf("t2 commit: %v", err)
	}

	err = t1.Commit(ctx)
	if !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("t1 commit: got %v, want kv.ErrConflict (phantom insert into scanned range)", err)
	}
}

func testWriteSkewAllowedUnderSnapshot(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	put(t, s, "a", "1")
	put(t, s, "b", "1")

	// Classic write skew: each transaction reads the other's write target.
	// Disjoint write sets, so Snapshot must let both commit.
	t1 := begin(t, s, kv.Snapshot)
	defer t1.Rollback()
	t2 := begin(t, s, kv.Snapshot)
	defer t2.Rollback()

	get(t, t1, "a")
	put(t, t1, "b", "t1")
	get(t, t2, "b")
	put(t, t2, "a", "t2")

	if err := t1.Commit(ctx); err != nil {
		t.Fatalf("t1 commit: %v", err)
	}
	if err := t2.Commit(ctx); err != nil {
		t.Fatalf("t2 commit under Snapshot should succeed (write skew), got %v", err)
	}
}

func testWriteSkewBlockedUnderSerializable(t *testing.T, s kv.TransactionalKV) {
	ctx := context.Background()
	put(t, s, "a", "1")
	put(t, s, "b", "1")

	t1 := begin(t, s, kv.SerializableSnapshot)
	defer t1.Rollback()
	t2 := begin(t, s, kv.SerializableSnapshot)
	defer t2.Rollback()

	get(t, t1, "a")
	put(t, t1, "b", "t1")
	get(t, t2, "b")
	put(t, t2, "a", "t2")

	if err := t1.Commit(ctx); err != nil {
		t.Fatalf("t1 commit: %v", err)
	}
	err := t2.Commit(ctx)
	if !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("t2 commit: got %v, want kv.ErrConflict (t2 read b, which t1 wrote)", err)
	}
}
