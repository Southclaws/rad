// Package kvslate adapts SlateDB (via the slatedb.io/slatedb-go uniffi
// bindings) to the kv.KV interface.
//
// Building or testing this package needs the native library:
//
//	CGO_LDFLAGS="-L<repo>/lib -Wl,-rpath,<repo>/lib" go test ./kv/kvslate/
//
// SlateDB stores keys as raw bytes and iterates in lexicographic order, so
// kv.Scan maps directly onto a SlateDB range scan. kv.Txn maps onto SlateDB
// transactions (Db.Begin / DbTransaction); commit-time transaction conflicts
// surface as kv.ErrConflict.
package kvslate

import (
	"context"
	"errors"
	"fmt"
	"strconv"
	"sync"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"

	slatedb "slatedb.io/slatedb-go/uniffi"
)

type Store struct {
	db       *slatedb.Db
	objStore *slatedb.ObjectStore

	mu       sync.Mutex
	cond     *sync.Cond
	active   int
	closing  bool
	closed   bool
	closeErr error
}

var _ kv.TransactionalKV = (*Store)(nil)

// Open opens (creating if needed) a SlateDB database named name on the object
// store at storeURL, e.g. "memory:///" or "file:///path/to/dir".
func Open(name, storeURL string) (*Store, error) {
	objStore, err := slatedb.ObjectStoreResolve(storeURL)
	if err != nil {
		return nil, err
	}
	builder := slatedb.NewDbBuilder(name, objStore)
	defer builder.Destroy()
	db, err := builder.Build()
	if err != nil {
		objStore.Destroy()
		return nil, err
	}
	store := &Store{db: db, objStore: objStore}
	store.cond = sync.NewCond(&store.mu)
	return store, nil
}

func (s *Store) Close() error {
	s.mu.Lock()
	if s.closing {
		for !s.closed {
			s.cond.Wait()
		}
		err := s.closeErr
		s.mu.Unlock()
		return err
	}
	s.closing = true
	for s.active != 0 {
		s.cond.Wait()
	}
	s.closeErr = s.db.Shutdown()
	s.db.Destroy()
	s.objStore.Destroy()
	s.closed = true
	s.cond.Broadcast()
	err := s.closeErr
	s.mu.Unlock()
	return err
}

func (s *Store) acquire() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closing {
		return errors.New("kvslate: store is closing")
	}
	s.active++
	return nil
}

func (s *Store) release() {
	s.mu.Lock()
	s.active--
	if s.active == 0 {
		s.cond.Broadcast()
	}
	s.mu.Unlock()
}

func (s *Store) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if err := ctx.Err(); err != nil {
		return nil, false, err
	}
	if err := s.acquire(); err != nil {
		return nil, false, err
	}
	defer s.release()
	v, err := s.db.Get(key)
	if err != nil {
		return nil, false, err
	}
	if v == nil {
		return nil, false, nil
	}
	return *v, true, nil
}

func (s *Store) Put(ctx context.Context, key []byte, value []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := s.acquire(); err != nil {
		return err
	}
	defer s.release()
	_, err := s.db.Put(key, value)
	return err
}

func (s *Store) Delete(ctx context.Context, key []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := s.acquire(); err != nil {
		return err
	}
	defer s.release()
	_, err := s.db.Delete(key)
	return err
}

func (s *Store) Scan(ctx context.Context, start []byte, end []byte) (kv.Iterator, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := s.acquire(); err != nil {
		return nil, err
	}
	it, err := s.db.Scan(keyRange(start, end))
	if err != nil {
		s.release()
		return nil, err
	}
	return &iterator{it: it, release: s.release}, nil
}

func keyRange(start, end []byte) slatedb.KeyRange {
	r := slatedb.KeyRange{StartInclusive: true, EndInclusive: false}
	if start != nil {
		r.Start = &start
	}
	if end != nil {
		r.End = &end
	}
	return r
}

// Begin starts a SlateDB transaction. Commit maps SlateDB's transaction
// conflict error to kv.ErrConflict (in the pinned binding the Transaction
// error category corresponds exactly to TransactionConflict).
func (s *Store) Begin(ctx context.Context, level kv.IsolationLevel) (kv.Txn, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := s.acquire(); err != nil {
		return nil, err
	}
	var lvl slatedb.IsolationLevel
	switch level {
	case kv.Snapshot:
		lvl = slatedb.IsolationLevelSnapshot
	case kv.SerializableSnapshot:
		lvl = slatedb.IsolationLevelSerializableSnapshot
	default:
		s.release()
		return nil, fmt.Errorf("kvslate: unknown isolation level %d", level)
	}
	tx, err := s.db.Begin(lvl)
	if err != nil {
		s.release()
		return nil, err
	}
	return &txn{tx: tx, release: s.release}, nil
}

type txn struct {
	tx      *slatedb.DbTransaction
	release func()
	done    bool
}

var (
	_ kv.Txn           = (*txn)(nil)
	_ kv.PositionedTxn = (*txn)(nil)
)

func (t *txn) BeginPosition() kv.DataPosition {
	return kv.DataPosition(strconv.FormatUint(t.tx.Seqnum(), 10))
}

func (t *txn) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if err := ctx.Err(); err != nil {
		return nil, false, err
	}
	v, err := t.tx.Get(key)
	if err != nil {
		return nil, false, err
	}
	if v == nil {
		return nil, false, nil
	}
	return *v, true, nil
}

func (t *txn) Put(ctx context.Context, key []byte, value []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	return t.tx.Put(key, value)
}

func (t *txn) Delete(ctx context.Context, key []byte) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	return t.tx.Delete(key)
}

func (t *txn) Scan(ctx context.Context, start []byte, end []byte) (kv.Iterator, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	it, err := t.tx.Scan(keyRange(start, end))
	if err != nil {
		return nil, err
	}
	return &iterator{it: it}, nil
}

// Commit uses SlateDB's default write options, which await durability.
func (t *txn) Commit(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	t.done = true
	defer t.finish()
	handle, err := t.tx.Commit()
	if handle != nil {
		handle.Destroy()
	}
	if err != nil && errors.Is(err, slatedb.ErrErrorTransaction) {
		return fmt.Errorf("%w: %w", kv.ErrConflict, err)
	}
	return err
}

func (t *txn) Rollback() error {
	if t.done {
		return nil
	}
	t.done = true
	defer t.finish()
	return t.tx.Rollback()
}

func (t *txn) finish() {
	t.tx.Destroy()
	if t.release != nil {
		t.release()
		t.release = nil
	}
}

type iterator struct {
	it      *slatedb.DbIterator
	key     []byte
	value   []byte
	err     error
	release func()
	once    sync.Once
}

func (it *iterator) Next() bool {
	if it.err != nil {
		return false
	}
	kvp, err := it.it.Next()
	if err != nil {
		it.err = err
		return false
	}
	if kvp == nil {
		return false
	}
	it.key = kvp.Key
	it.value = kvp.Value
	return true
}

func (it *iterator) Key() []byte   { return it.key }
func (it *iterator) Value() []byte { return it.value }
func (it *iterator) Err() error    { return it.err }

func (it *iterator) Close() error {
	it.once.Do(func() {
		it.it.Destroy()
		if it.release != nil {
			it.release()
		}
	})
	return nil
}
