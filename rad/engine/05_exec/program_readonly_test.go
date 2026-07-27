package exec

import (
	"context"
	"sync"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

type isolationTrackingStore struct {
	kv.TransactionalKV
	mu      sync.Mutex
	levels  []kv.IsolationLevel
	commits int
}

func (s *isolationTrackingStore) Begin(ctx context.Context, level kv.IsolationLevel) (kv.Txn, error) {
	txn, err := s.TransactionalKV.Begin(ctx, level)
	if err != nil {
		return nil, err
	}
	s.mu.Lock()
	s.levels = append(s.levels, level)
	s.mu.Unlock()
	return &isolationTrackingTxn{Txn: txn, store: s}, nil
}

func (s *isolationTrackingStore) reset() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.levels = nil
	s.commits = 0
}

func (s *isolationTrackingStore) observations() ([]kv.IsolationLevel, int) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]kv.IsolationLevel(nil), s.levels...), s.commits
}

type isolationTrackingTxn struct {
	kv.Txn
	store *isolationTrackingStore
}

func (tx *isolationTrackingTxn) Commit(ctx context.Context) error {
	err := tx.Txn.Commit(ctx)
	if err == nil {
		tx.store.mu.Lock()
		tx.store.commits++
		tx.store.mu.Unlock()
	}
	return err
}

func TestExecuteProgramReadOnlyFastPathUsesSnapshotWithoutCommit(t *testing.T) {
	ctx := context.Background()
	base, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = base.Close() })
	store := &isolationTrackingStore{TransactionalKV: base}
	cat := catalog.New(store)
	eng := New(store, cat)
	if _, err := cat.CreateTable(ctx, model.TableDef{
		Name: "items",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "items", lir.Row{"id": lir.Int64(1), "value": lir.Text("one")}); err != nil {
		t.Fatal(err)
	}

	store.reset()
	result, err := eng.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "read", Kind: execprogram.Query,
		Rel: lir.Query{Card: lir.CardMany, Root: lir.Order{
			Input: lir.Scan{Table: "items", Scope: "i"},
			Terms: []lir.OrderTerm{{Expr: qcol("i", "id")}},
		}},
	}}}, execprogram.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if result.Result.Kind != lir.DatumArray {
		t.Fatalf("read result = %#v", result.Result)
	}
	levels, commits := store.observations()
	if commits != 0 {
		t.Fatalf("read-only program committed %d transactions", commits)
	}
	if len(levels) == 0 || levels[len(levels)-1] != kv.Snapshot {
		t.Fatalf("read-only execution levels = %v, final transaction must be snapshot", levels)
	}

	store.reset()
	_, err = eng.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "write", Kind: execprogram.Create, Table: "items",
		Rel: lir.Query{Card: lir.CardMany, Root: lir.Rows{
			Scope: "r",
			Columns: []lir.RowsCol{
				{Name: "id", Kind: lir.KindInt64},
				{Name: "value", Kind: lir.KindText},
			},
			Values: [][]any{{int64(2), "two"}},
		}},
	}}}, execprogram.Options{})
	if err != nil {
		t.Fatal(err)
	}
	levels, commits = store.observations()
	if commits != 1 {
		t.Fatalf("write program commits = %d, want 1", commits)
	}
	if len(levels) == 0 || levels[len(levels)-1] != kv.SerializableSnapshot {
		t.Fatalf("write execution levels = %v, final transaction must be serializable", levels)
	}
}
