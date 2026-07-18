package exec

// CONTENT: dumps the physical plan and the SlateDB KV traffic for the blog
// post's users->posts crossing query. Run: go test ./rad/engine/05_exec/ -run
// TestBlogDump -v.

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
	"github.com/Southclaws/rad/rad/engine/04_planner/explain"
)

func TestBlogDump(t *testing.T) {
	ctx := context.Background()
	store, err := kvslate.Open("blogdump", "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })

	var ops []string
	on := false
	logged := loggingStore{TransactionalKV: store, ops: &ops, on: &on}

	cat := catalog.New(logged)
	mk := func(def model.TableDef) {
		if _, err := cat.CreateTable(ctx, def); err != nil {
			t.Fatal(err)
		}
	}
	mk(model.TableDef{
		Name: "users", PrimaryKey: []string{"id"},
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "name", Type: model.TypeText},
		},
	})
	mk(model.TableDef{
		Name: "posts", PrimaryKey: []string{"id"},
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "user_id", Type: model.TypeInt64},
			{Name: "title", Type: model.TypeText},
			{Name: "created_at", Type: model.TypeInt64},
		},
		Indexes:     []model.IndexDef{{Name: "posts_user_id_idx", Columns: []string{"user_id"}}},
		ForeignKeys: []model.ForeignKeyDef{{Name: "posts_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}}},
	})

	eng := New(logged, cat)
	mustIns := func(table string, row lir.Row) {
		if err := eng.Insert(ctx, table, row); err != nil {
			t.Fatal(err)
		}
	}
	mustIns("users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Donald D. Chamberlin")})
	mustIns("posts", lir.Row{"id": lir.Int64(1), "user_id": lir.Int64(1), "title": lir.Text("SQUARE: Specifying Queries as Relational Expressions"), "created_at": lir.Int64(94694400)})
	mustIns("posts", lir.Row{"id": lir.Int64(2), "user_id": lir.Int64(1), "title": lir.Text("SEQUEL: A Structured English Query Language"), "created_at": lir.Int64(136598400)})

	// The crossing query: each user with a nested array of their posts after
	// 1974-01-01 (unix 126230400), newest ordering by created_at.
	mine := lir.Filter{
		Input: qscan("posts", "p"),
		Pred: qand(
			qeq(qcol("p", "user_id"), qcol("u", "id")),
			lir.Binary{Op: lir.OpGt, L: qcol("p", "created_at"), R: qlit(int64(126230400))},
		),
	}
	postObjs := lir.Project{Input: mine, Scope: "po", Fields: []lir.ProjField{
		{As: "title", Expr: qcol("p", "title")},
		{As: "created_at", Expr: qcol("p", "created_at")},
	}}
	sorted := lir.Order{Input: postObjs, Terms: []lir.OrderTerm{{Expr: qcol("po", "created_at")}}}
	out := lir.Project{Input: qscan("users", "u"), Scope: "r", Fields: []lir.ProjField{
		{As: "id", Expr: qcol("u", "id")},
		{As: "name", Expr: qcol("u", "name")},
		{As: "posts", Expr: lir.Array{Rel: sorted}},
	}}
	q := lir.Query{Card: lir.CardMany, Root: lir.Order{Input: out, Terms: []lir.OrderTerm{{Expr: qcol("r", "id")}}}}

	// Physical plan (post bind + planning).
	bq, err := binder.Bind(ctx, cat, q)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("\n===== PHYSICAL PLAN =====\n%s", explain.PrintPlan(planner.PlanQuery(bq)))

	// Execute, logging the data-plane KV traffic.
	on = true
	res, err := eng.Execute(ctx, q)
	on = false
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("\n===== KV OPS TO SLATEDB (%d) =====\n%s", len(ops), strings.Join(ops, "\n"))
	pretty, _ := json.MarshalIndent(jsonish(res), "", "  ")
	t.Logf("\n===== RESULT =====\n%s", pretty)
}

type loggingStore struct {
	kv.TransactionalKV
	ops *[]string
	on  *bool
}

func (l loggingStore) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	l.record("GET  ", key, nil)
	return l.TransactionalKV.Get(ctx, key)
}

func (l loggingStore) Scan(ctx context.Context, start, end []byte) (kv.Iterator, error) {
	l.record("SCAN ", start, end)
	return l.TransactionalKV.Scan(ctx, start, end)
}

func (l loggingStore) Begin(ctx context.Context, lvl kv.IsolationLevel) (kv.Txn, error) {
	txn, err := l.TransactionalKV.Begin(ctx, lvl)
	if err != nil {
		return nil, err
	}
	return loggingTxn{Txn: txn, ops: l.ops, on: l.on}, nil
}

func (l loggingStore) record(op string, a, b []byte) {
	if *l.on && dataKey(a) {
		*l.ops = append(*l.ops, op+keyDesc(a, b))
	}
}

type loggingTxn struct {
	kv.Txn
	ops *[]string
	on  *bool
}

func (l loggingTxn) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if *l.on && dataKey(key) {
		*l.ops = append(*l.ops, "GET  "+keyDesc(key, nil))
	}
	return l.Txn.Get(ctx, key)
}

func (l loggingTxn) Scan(ctx context.Context, start, end []byte) (kv.Iterator, error) {
	if *l.on && dataKey(start) {
		*l.ops = append(*l.ops, "SCAN "+keyDesc(start, end))
	}
	return l.Txn.Scan(ctx, start, end)
}

// keyDesc renders a key (and, for a scan, its end bound) with printable bytes
// inline and everything else hex-escaped.
func keyDesc(a, b []byte) string {
	if b == nil {
		return keyStr(a)
	}
	return "[" + keyStr(a) + " .. " + keyStr(b) + ")"
}

func keyStr(k []byte) string {
	var s strings.Builder
	for _, c := range k {
		if c >= 0x20 && c < 0x7f {
			s.WriteByte(c)
		} else {
			fmt.Fprintf(&s, "\\x%02x", c)
		}
	}
	return s.String()
}
