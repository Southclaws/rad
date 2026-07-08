// Package kv defines a minimal ordered key-value storage abstraction.
//
// Implementations must preserve lexicographic byte ordering of keys so that
// range scans over encoded keys return rows in tuple order.
//
// KV is the plain read/write surface; Txn adds transaction lifecycle on top
// of the same surface, so code that operates on a KV works unchanged inside
// a transaction. TransactionalKV is what backends implement.
package kv

import (
	"context"
	"errors"
)

// ErrConflict is returned by Txn.Commit when the transaction lost an
// optimistic concurrency race and must be retried by the caller (with fresh
// reads) or abandoned. Check with errors.Is.
var ErrConflict = errors.New("kv: transaction conflict")

// IsolationLevel selects the conflict detection a transaction commits under.
type IsolationLevel int

const (
	// Snapshot gives stable reads as of Begin and detects write-write
	// conflicts only. Susceptible to write skew.
	Snapshot IsolationLevel = iota

	// SerializableSnapshot additionally detects read-write conflicts: point
	// reads and the *requested* bounds of scans are validated against keys
	// committed by others after Begin, so phantom inserts into a scanned
	// range conflict even if the scan returned nothing.
	SerializableSnapshot
)

// KV is an ordered key-value store.
type KV interface {
	// Get returns the value for key. The second return is false if the key
	// does not exist.
	Get(ctx context.Context, key []byte) ([]byte, bool, error)

	// Put sets the value for key, overwriting any existing value.
	Put(ctx context.Context, key []byte, value []byte) error

	// Delete removes key. Deleting a missing key is not an error.
	Delete(ctx context.Context, key []byte) error

	// Scan iterates keys in [start, end) in ascending lexicographic order.
	// A nil start means "from the beginning"; a nil end means "to the end".
	Scan(ctx context.Context, start []byte, end []byte) (Iterator, error)
}

// Txn is a transaction over a KV. Reads see a stable snapshot taken at Begin
// overlaid with the transaction's own writes; writes are buffered in memory
// until Commit. A Txn is not safe for concurrent use by multiple goroutines.
type Txn interface {
	KV

	// Commit atomically and durably applies the buffered writes. It returns
	// an error wrapping ErrConflict if the transaction lost a concurrency
	// race under its isolation level. The Txn is unusable afterwards either
	// way. Iterators from the Txn must be closed before Commit.
	Commit(ctx context.Context) error

	// Rollback discards the transaction. It is a no-op after Commit or a
	// prior Rollback, so `defer txn.Rollback()` is always safe.
	Rollback() error
}

// TransactionalKV is an ordered key-value store that supports optimistic
// transactions alongside plain (implicitly single-op transactional) access.
type TransactionalKV interface {
	KV

	// Begin starts a transaction reading from the current committed state.
	Begin(ctx context.Context, level IsolationLevel) (Txn, error)
}

// Iterator walks scan results. Usage:
//
//	for it.Next() {
//	    _ = it.Key()
//	    _ = it.Value()
//	}
//	if err := it.Err(); err != nil { ... }
//	_ = it.Close()
//
// Key and Value are only valid until the next call to Next.
type Iterator interface {
	Next() bool
	Key() []byte
	Value() []byte
	Err() error
	Close() error
}
