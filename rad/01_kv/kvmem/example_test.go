package kvmem_test

import (
	"context"
	"errors"
	"fmt"

	kv "rad/rad/01_kv"
	"rad/rad/01_kv/kvmem"
)

// The kv.KV surface every backend provides: ordered puts, gets, and
// [start, end) range scans in lexicographic key order.
func Example() {
	ctx := context.Background()
	store := kvmem.New()

	for _, k := range []string{"b", "a", "c"} {
		if err := store.Put(ctx, []byte(k), []byte("value-"+k)); err != nil {
			panic(err)
		}
	}

	it, err := store.Scan(ctx, []byte("a"), []byte("c")) // end is exclusive
	if err != nil {
		panic(err)
	}
	defer it.Close()
	for it.Next() {
		fmt.Printf("%s = %s\n", it.Key(), it.Value())
	}
	// Output:
	// a = value-a
	// b = value-b
}

// The kv.TransactionalKV surface: optimistic transactions with snapshot
// reads and buffered writes. Under SerializableSnapshot a transaction whose
// reads were invalidated by a concurrent commit loses at Commit with
// kv.ErrConflict — the caller retries with fresh reads.
func Example_transactions() {
	ctx := context.Background()
	store := kvmem.New()
	_ = store.Put(ctx, []byte("balance"), []byte("100"))

	txn, err := store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		panic(err)
	}
	defer txn.Rollback()

	// The transaction bases a decision on this read...
	v, _, _ := txn.Get(ctx, []byte("balance"))
	fmt.Printf("txn read balance=%s\n", v)

	// ...but a rival commits a new balance before it finishes.
	_ = store.Put(ctx, []byte("balance"), []byte("0"))

	_ = txn.Put(ctx, []byte("withdrawal"), []byte("100"))
	err = txn.Commit(ctx)
	fmt.Println("conflict:", errors.Is(err, kv.ErrConflict))
	// Output:
	// txn read balance=100
	// conflict: true
}
