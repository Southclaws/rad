package frontend_test

import (
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
)

// memStore opens a fresh in-memory SlateDB for one test.
func memStore(t *testing.T) kv.TransactionalKV {
	t.Helper()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	return store
}
