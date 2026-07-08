// Package kvmem provides a pure-Go in-memory implementation of
// kv.TransactionalKV for tests. It mirrors SlateDB's transaction semantics —
// snapshot reads as of Begin, buffered writes, optimistic commit-time
// conflict detection (write-write always; read keys and requested scan
// ranges under SerializableSnapshot) — so transaction behaviour can be
// tested without the native library. It is not optimized: values are
// versioned per key and scans snapshot the matching range up front.
package kvmem

import (
	"bytes"
	"context"
	"fmt"
	"sort"
	"sync"

	"rad/rad/01_kv"
)

type version struct {
	seq       uint64
	value     []byte
	tombstone bool
}

// committedWrite records the key set of one committed write (a transaction
// or a single direct Put/Delete) for conflict checks against transactions
// that began before it committed.
type committedWrite struct {
	seq  uint64
	keys map[string]struct{}
}

type Store struct {
	mu        sync.Mutex
	seq       uint64               // last committed sequence number
	data      map[string][]version // per-key history, ascending seq
	committed []committedWrite     // ascending seq, trimmed as txns finish
	active    map[*Txn]uint64      // startSeq of each in-flight transaction
}

var _ kv.TransactionalKV = (*Store)(nil)

func New() *Store {
	return &Store{
		data:   make(map[string][]version),
		active: make(map[*Txn]uint64),
	}
}

// readAt returns the value visible at snapshot sequence seq.
func (s *Store) readAt(key string, seq uint64) ([]byte, bool) {
	for i := len(s.data[key]) - 1; i >= 0; i-- {
		v := s.data[key][i]
		if v.seq <= seq {
			if v.tombstone {
				return nil, false
			}
			return v.value, true
		}
	}
	return nil, false
}

// applyLocked commits a write set at the next sequence number and records it
// for conflict detection. Caller holds s.mu.
func (s *Store) applyLocked(writes map[string]version) {
	s.seq++
	keys := make(map[string]struct{}, len(writes))
	for k, v := range writes {
		v.seq = s.seq
		s.data[k] = append(s.data[k], v)
		keys[k] = struct{}{}
	}
	s.committed = append(s.committed, committedWrite{seq: s.seq, keys: keys})
	s.trimLocked()
}

// trimLocked drops committed entries that no active transaction can conflict
// with (conflicts only consider entries with seq > the transaction's
// startSeq). Mirrors SlateDB's recent_committed_txns GC.
func (s *Store) trimLocked() {
	minStart := s.seq
	for _, start := range s.active {
		if start < minStart {
			minStart = start
		}
	}
	i := 0
	for i < len(s.committed) && s.committed[i].seq <= minStart {
		i++
	}
	s.committed = s.committed[i:]
}

func (s *Store) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if err := ctx.Err(); err != nil {
		return nil, false, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	v, ok := s.readAt(string(key), s.seq)
	if !ok {
		return nil, false, nil
	}
	return bytes.Clone(v), true, nil
}

func (s *Store) Put(ctx context.Context, key []byte, value []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.applyLocked(map[string]version{string(key): {value: bytes.Clone(value)}})
	return nil
}

func (s *Store) Delete(ctx context.Context, key []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	s.applyLocked(map[string]version{string(key): {tombstone: true}})
	return nil
}

// scanAt materializes the entries of [start, end) visible at seq, in order.
// Caller holds s.mu. extra overlays transaction-buffered writes.
func (s *Store) scanAt(start, end []byte, seq uint64, extra map[string]version) []entry {
	inRange := func(k string) bool {
		return (start == nil || k >= string(start)) && (end == nil || k < string(end))
	}
	merged := make(map[string][]byte)
	for k := range s.data {
		if !inRange(k) {
			continue
		}
		if v, ok := s.readAt(k, seq); ok {
			merged[k] = v
		}
	}
	for k, v := range extra {
		if !inRange(k) {
			continue
		}
		if v.tombstone {
			delete(merged, k)
		} else {
			merged[k] = v.value
		}
	}

	keys := make([]string, 0, len(merged))
	for k := range merged {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	entries := make([]entry, len(keys))
	for i, k := range keys {
		entries[i] = entry{key: []byte(k), value: bytes.Clone(merged[k])}
	}
	return entries
}

func (s *Store) Scan(ctx context.Context, start []byte, end []byte) (kv.Iterator, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	return &iterator{entries: s.scanAt(start, end, s.seq, nil), pos: -1}, nil
}

func (s *Store) Begin(ctx context.Context, level kv.IsolationLevel) (kv.Txn, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	switch level {
	case kv.Snapshot, kv.SerializableSnapshot:
	default:
		return nil, fmt.Errorf("kvmem: unknown isolation level %d", level)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	t := &Txn{
		s:        s,
		level:    level,
		startSeq: s.seq,
		writes:   make(map[string]version),
	}
	if level == kv.SerializableSnapshot {
		t.reads = make(map[string]struct{})
	}
	s.active[t] = t.startSeq
	return t, nil
}

// scanRange is the requested bounds of a transactional scan, tracked for
// phantom detection under SerializableSnapshot.
type scanRange struct {
	start, end []byte
}

func (r scanRange) contains(key string) bool {
	return (r.start == nil || key >= string(r.start)) && (r.end == nil || key < string(r.end))
}

type Txn struct {
	s        *Store
	level    kv.IsolationLevel
	startSeq uint64
	writes   map[string]version  // buffered until Commit
	reads    map[string]struct{} // point reads (SerializableSnapshot only)
	ranges   []scanRange         // scanned bounds (SerializableSnapshot only)
	done     bool
}

var _ kv.Txn = (*Txn)(nil)

func (t *Txn) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if err := ctx.Err(); err != nil {
		return nil, false, err
	}
	if t.done {
		return nil, false, fmt.Errorf("kvmem: transaction already completed")
	}
	// Reads of missing keys are tracked too: a later insert of this key by
	// another transaction is a read-write conflict.
	if t.reads != nil {
		t.reads[string(key)] = struct{}{}
	}
	if w, ok := t.writes[string(key)]; ok {
		if w.tombstone {
			return nil, false, nil
		}
		return bytes.Clone(w.value), true, nil
	}
	t.s.mu.Lock()
	defer t.s.mu.Unlock()
	v, ok := t.s.readAt(string(key), t.startSeq)
	if !ok {
		return nil, false, nil
	}
	return bytes.Clone(v), true, nil
}

func (t *Txn) Put(ctx context.Context, key []byte, value []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if t.done {
		return fmt.Errorf("kvmem: transaction already completed")
	}
	t.writes[string(key)] = version{value: bytes.Clone(value)}
	return nil
}

func (t *Txn) Delete(ctx context.Context, key []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if t.done {
		return fmt.Errorf("kvmem: transaction already completed")
	}
	t.writes[string(key)] = version{tombstone: true}
	return nil
}

func (t *Txn) Scan(ctx context.Context, start []byte, end []byte) (kv.Iterator, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if t.done {
		return nil, fmt.Errorf("kvmem: transaction already completed")
	}
	// The *requested* range is tracked, not the returned keys, so an empty
	// scan still conflicts with a concurrent insert into the range.
	if t.reads != nil {
		t.ranges = append(t.ranges, scanRange{start: bytes.Clone(start), end: bytes.Clone(end)})
	}
	t.s.mu.Lock()
	defer t.s.mu.Unlock()
	return &iterator{entries: t.s.scanAt(start, end, t.startSeq, t.writes), pos: -1}, nil
}

func (t *Txn) Commit(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if t.done {
		return fmt.Errorf("kvmem: transaction already completed")
	}
	t.done = true
	t.s.mu.Lock()
	defer t.s.mu.Unlock()
	delete(t.s.active, t)
	defer t.s.trimLocked()

	for _, c := range t.s.committed {
		if c.seq <= t.startSeq {
			continue
		}
		for k := range t.writes {
			if _, hit := c.keys[k]; hit {
				return fmt.Errorf("%w: write-write on %q", kv.ErrConflict, k)
			}
		}
		if t.level != kv.SerializableSnapshot {
			continue
		}
		for k := range t.reads {
			if _, hit := c.keys[k]; hit {
				return fmt.Errorf("%w: read key %q overwritten", kv.ErrConflict, k)
			}
		}
		for _, r := range t.ranges {
			for k := range c.keys {
				if r.contains(k) {
					return fmt.Errorf("%w: scanned range contains committed write %q", kv.ErrConflict, k)
				}
			}
		}
	}

	if len(t.writes) > 0 {
		t.s.applyLocked(t.writes)
	}
	return nil
}

func (t *Txn) Rollback() error {
	if t.done {
		return nil
	}
	t.done = true
	t.s.mu.Lock()
	defer t.s.mu.Unlock()
	delete(t.s.active, t)
	t.s.trimLocked()
	return nil
}

type entry struct {
	key   []byte
	value []byte
}

type iterator struct {
	entries []entry
	pos     int
}

func (it *iterator) Next() bool {
	if it.pos+1 >= len(it.entries) {
		return false
	}
	it.pos++
	return true
}

func (it *iterator) Key() []byte   { return it.entries[it.pos].key }
func (it *iterator) Value() []byte { return it.entries[it.pos].value }
func (it *iterator) Err() error    { return nil }
func (it *iterator) Close() error  { return nil }
