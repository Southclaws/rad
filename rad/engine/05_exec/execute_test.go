package exec

// Relation-graph executor tests: the forcing query end to end with exact
// KV-operation counts (the deduplicated-correlated-execution proof),
// batched ≡ nested equivalence, three-valued logic at the row level,
// aggregate empty-set rules, ordered-index pushdown, range scans, and the
// whole thing inside a transaction (interleaved Gets with an open iterator
// — the kvslate risk item).

import (
	"context"
	"fmt"
	"reflect"
	"strings"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// countingStore tallies data-plane Gets and Scans — row and index keys,
// not catalog reads — through both the direct view and transactions.
type countingStore struct {
	kv.TransactionalKV
	gets, scans *int
}

func dataKey(key []byte) bool {
	s := string(key)
	return strings.HasPrefix(s, "/rad/data/") || strings.HasPrefix(s, "/rad/index/")
}

func (c countingStore) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if dataKey(key) {
		*c.gets++
	}
	return c.TransactionalKV.Get(ctx, key)
}

func (c countingStore) Scan(ctx context.Context, start, end []byte) (kv.Iterator, error) {
	if dataKey(start) {
		*c.scans++
	}
	return c.TransactionalKV.Scan(ctx, start, end)
}

func (c countingStore) Begin(ctx context.Context, lvl kv.IsolationLevel) (kv.Txn, error) {
	txn, err := c.TransactionalKV.Begin(ctx, lvl)
	if err != nil {
		return nil, err
	}
	return countingTxn{Txn: txn, gets: c.gets, scans: c.scans}, nil
}

type countingTxn struct {
	kv.Txn
	gets, scans *int
}

func (c countingTxn) Get(ctx context.Context, key []byte) ([]byte, bool, error) {
	if dataKey(key) {
		*c.gets++
	}
	return c.Txn.Get(ctx, key)
}

func (c countingTxn) Scan(ctx context.Context, start, end []byte) (kv.Iterator, error) {
	if dataKey(start) {
		*c.scans++
	}
	return c.Txn.Scan(ctx, start, end)
}

// lirSetup builds the tracker schema, seeds the forcing-query data, and
// returns the engine plus the KV op counters.
func lirSetup(t *testing.T) (*Engine, context.Context, *int, *int) {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	gets, scans := new(int), new(int)
	counted := countingStore{TransactionalKV: store, gets: gets, scans: scans}

	cat := catalog.New(counted)
	mk := func(def catalog.TableDef) {
		t.Helper()
		if _, err := cat.CreateTable(ctx, def); err != nil {
			t.Fatal(err)
		}
	}
	mk(catalog.TableDef{Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "name", Type: catalog.TypeText}},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "users_name_uq", Columns: []string{"name"}, Unique: true}}})
	mk(catalog.TableDef{Name: "boards",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "name", Type: catalog.TypeText},
			{Name: "owner_id", Type: catalog.TypeText}},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "boards_owner_fk", Columns: []string{"owner_id"}, RefTable: "users", RefColumns: []string{"id"}}}})
	mk(catalog.TableDef{Name: "tasks",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "board_id", Type: catalog.TypeText},
			{Name: "title", Type: catalog.TypeText},
			{Name: "status", Type: catalog.TypeText},
			{Name: "priority", Type: catalog.TypeInt64},
			{Name: "estimate", Type: catalog.TypeFloat64, Nullable: true},
			{Name: "assignee_id", Type: catalog.TypeText, Nullable: true}},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "tasks_board_status_idx", Columns: []string{"board_id", "status"}}},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "tasks_board_fk", Columns: []string{"board_id"}, RefTable: "boards", RefColumns: []string{"id"}},
			{Name: "tasks_assignee_fk", Columns: []string{"assignee_id"}, RefTable: "users", RefColumns: []string{"id"}}}})
	mk(catalog.TableDef{Name: "comments",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "task_id", Type: catalog.TypeText},
			{Name: "body", Type: catalog.TypeText}},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "comments_task_idx", Columns: []string{"task_id"}}},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "comments_task_fk", Columns: []string{"task_id"}, RefTable: "tasks", RefColumns: []string{"id"}}}})

	eng := New(counted, cat)
	ins := func(table string, row lir.Row) {
		t.Helper()
		if err := eng.Insert(ctx, table, row); err != nil {
			t.Fatal(err)
		}
	}
	ins("users", lir.Row{"id": lir.Text("ada"), "name": lir.Text("Ada")})
	ins("users", lir.Row{"id": lir.Text("bob"), "name": lir.Text("Bob")})
	ins("boards", lir.Row{"id": lir.Text("b1"), "name": lir.Text("Launch"), "owner_id": lir.Text("ada")})
	ins("boards", lir.Row{"id": lir.Text("b2"), "name": lir.Text("Infra"), "owner_id": lir.Text("ada")})
	ins("boards", lir.Row{"id": lir.Text("b3"), "name": lir.Text("Empty"), "owner_id": lir.Text("bob")})
	ins("tasks", lir.Row{"id": lir.Text("t1"), "board_id": lir.Text("b1"), "title": lir.Text("ship"),
		"status": lir.Text("open"), "priority": lir.Int64(3), "assignee_id": lir.Text("bob"), "estimate": lir.Float64(2)})
	ins("tasks", lir.Row{"id": lir.Text("t2"), "board_id": lir.Text("b1"), "title": lir.Text("write"),
		"status": lir.Text("open"), "priority": lir.Int64(5)}) // no assignee, no estimate
	ins("tasks", lir.Row{"id": lir.Text("t3"), "board_id": lir.Text("b1"), "title": lir.Text("done"),
		"status": lir.Text("done"), "priority": lir.Int64(9), "assignee_id": lir.Text("ada")})
	ins("tasks", lir.Row{"id": lir.Text("t4"), "board_id": lir.Text("b2"), "title": lir.Text("rack"),
		"status": lir.Text("open"), "priority": lir.Int64(1), "assignee_id": lir.Text("ada"), "estimate": lir.Float64(8)})
	ins("comments", lir.Row{"id": lir.Text("c1"), "task_id": lir.Text("t1"), "body": lir.Text("nice")})
	ins("comments", lir.Row{"id": lir.Text("c2"), "task_id": lir.Text("t1"), "body": lir.Text("ship it")})
	ins("comments", lir.Row{"id": lir.Text("c3"), "task_id": lir.Text("t4"), "body": lir.Text("racked")})

	*gets, *scans = 0, 0
	return eng, ctx, gets, scans
}

// -
// unbound construction helpers
// -

func qcol(scope, name string) lir.Column { return lir.Column{Scope: scope, Name: name} }
func qlit(v any) lir.Literal             { return lir.Literal{Raw: v} }
func qeq(l, r lir.Expr) lir.Expr         { return lir.Binary{Op: lir.OpEq, L: l, R: r} }
func qand(l, r lir.Expr) lir.Expr        { return lir.Binary{Op: lir.OpAnd, L: l, R: r} }
func qscan(tbl, scope string) lir.Scan   { return lir.Scan{Table: tbl, Scope: scope} }
func qfilter(in lir.Relation, p lir.Expr) lir.Filter {
	return lir.Filter{Input: in, Pred: p}
}
func many(root lir.Relation) lir.Query {
	return lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: root,
		Terms: []lir.OrderTerm{{Expr: qlit(true)}},
	}}
}

// jsonish flattens a Datum for readable assertions.
func jsonish(d lir.Datum) any {
	switch d.Kind {
	case lir.DatumNull:
		return nil
	case lir.DatumScalar:
		v := d.Scalar
		switch v.Type {
		case catalog.TypeText:
			return v.Text
		case catalog.TypeInt64:
			return v.Int64
		case catalog.TypeFloat64:
			return v.Float64
		case catalog.TypeBool:
			return v.Bool
		}
		return v
	case lir.DatumObject:
		m := map[string]any{}
		for _, f := range d.Fields {
			m[f.Name] = jsonish(f.Datum)
		}
		return m
	default:
		out := make([]any, len(d.Elems))
		for i, e := range d.Elems {
			out[i] = jsonish(e)
		}
		return out
	}
}

// forcingQ is the acceptance query: boards → owner → open tasks by priority
// desc → assignee + comment count.
func forcingQ() lir.Query {
	owner := qfilter(qscan("users", "o"), qeq(qcol("o", "id"), qcol("b", "owner_id")))
	assignee := qfilter(qscan("users", "a"), qeq(qcol("a", "id"), qcol("t", "assignee_id")))
	commentCount := lir.Aggregate{
		Input: qfilter(qscan("comments", "c"), qeq(qcol("c", "task_id"), qcol("t", "id"))),
		Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	}
	openTasks := lir.Project{
		Input: lir.Slice{
			Input: lir.Order{
				Input: qfilter(qscan("tasks", "t"),
					qand(qeq(qcol("t", "board_id"), qcol("b", "id")),
						qeq(qcol("t", "status"), qlit("open")))),
				Terms: []lir.OrderTerm{{Expr: qcol("t", "priority"), Desc: true}},
			},
			Limit: new(int(20)),
		},
		Fields: []lir.ProjField{
			{As: "id", Expr: qcol("t", "id")},
			{As: "title", Expr: qcol("t", "title")},
			{As: "assignee", Expr: lir.First{Rel: assignee}},
			{As: "comment_count", Expr: lir.Scalar{Rel: commentCount}},
		},
	}
	return many(lir.Project{
		Input: lir.Order{Input: qscan("boards", "b"),
			Terms: []lir.OrderTerm{{Expr: qcol("b", "id")}}},
		Fields: []lir.ProjField{
			{As: "id", Expr: qcol("b", "id")},
			{As: "owner", Expr: lir.First{Rel: owner}},
			{As: "tasks", Expr: lir.Array{Rel: openTasks}},
		},
	})
}

var forcingWant = []any{
	map[string]any{"id": "b1",
		"owner": map[string]any{"id": "ada", "name": "Ada"},
		"tasks": []any{
			map[string]any{"id": "t2", "title": "write", "assignee": nil, "comment_count": int64(0)},
			map[string]any{"id": "t1", "title": "ship",
				"assignee": map[string]any{"id": "bob", "name": "Bob"}, "comment_count": int64(2)},
		}},
	map[string]any{"id": "b2",
		"owner": map[string]any{"id": "ada", "name": "Ada"},
		"tasks": []any{
			map[string]any{"id": "t4", "title": "rack",
				"assignee": map[string]any{"id": "ada", "name": "Ada"}, "comment_count": int64(1)},
		}},
	map[string]any{"id": "b3",
		"owner": map[string]any{"id": "bob", "name": "Bob"},
		"tasks": []any{}},
}

func TestExecuteForcingQuery(t *testing.T) {
	eng, ctx, gets, scans := lirSetup(t)

	d, err := eng.Execute(ctx, forcingQ())
	if err != nil {
		t.Fatal(err)
	}
	if got := jsonish(d); !reflect.DeepEqual(got, forcingWant) {
		t.Fatalf("forcing query result:\n got %#v\nwant %#v", got, forcingWant)
	}

	// The op-count proof of deduplicated correlated execution:
	//   1 boards table scan
	//   2 owner point gets      (ada deduped across b1+b2)
	//   3 task index scans      (one per distinct board) + 3 base-row gets
	//   2 assignee point gets   (per-board batches: {bob}, {ada}; NULL free)
	//   3 comment index scans   (t2, t1, t4) + 3 base-row gets
	if *scans != 7 {
		t.Errorf("scans = %d, want 7", *scans)
	}
	if *gets != 10 {
		t.Errorf("gets = %d, want 10", *gets)
	}
}

// Batched and nested execution are result-equivalent by construction —
// and nested costs strictly more storage work.
func TestExecuteBatchedEquivalentToNested(t *testing.T) {
	eng, ctx, gets, scans := lirSetup(t)

	batched, err := eng.Execute(ctx, forcingQ())
	if err != nil {
		t.Fatal(err)
	}
	bGets, bScans := *gets, *scans
	*gets, *scans = 0, 0

	nested, err := eng.ExecuteNested(ctx, forcingQ())
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(jsonish(batched), jsonish(nested)) {
		t.Fatalf("batched ≠ nested:\n batched %#v\n nested  %#v", jsonish(batched), jsonish(nested))
	}
	if *gets <= bGets {
		t.Errorf("nested gets = %d, batched = %d — dedup saved nothing?", *gets, bGets)
	}
	_ = bScans
}

// The whole pipeline runs inside a transaction: index-range iteration
// interleaves Gets with the open iterator against a kvslate txn — the risk
// item, pinned here.
func TestExecuteInsideTransaction(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	err := eng.Txn(ctx, func(tx *Tx) error {
		if err := tx.Insert(ctx, "comments", lir.Row{
			"id": lir.Text("c4"), "task_id": lir.Text("t2"), "body": lir.Text("mid-txn")}); err != nil {
			return err
		}
		d, err := tx.Execute(ctx, forcingQ())
		if err != nil {
			return err
		}
		// The txn sees its own uncommitted write: t2 now has one comment.
		b1 := jsonish(d).([]any)[0].(map[string]any)
		t2 := b1["tasks"].([]any)[0].(map[string]any)
		if t2["comment_count"] != int64(1) {
			return fmt.Errorf("t2 comment_count = %v inside txn, want 1", t2["comment_count"])
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

// NULLs are distinct under unique indexes: any number of rows may leave a
// unique column NULL, while non-NULL duplicates stay rejected — on insert,
// on update, and through backfill.
func TestUniqueIndexNullsDistinct(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	if _, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "invites",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "email", Type: catalog.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}

	// Two pending invites with no email yet: fine.
	for _, id := range []string{"i1", "i2"} {
		if err := eng.Insert(ctx, "invites", lir.Row{"id": lir.Text(id)}); err != nil {
			t.Fatal(err)
		}
	}
	// Backfilling the unique index over the NULL pair succeeds.
	if err := eng.CreateIndexWithBackfill(ctx, "invites", catalog.IndexDef{
		Name: "invites_email_uq", Columns: []string{"email"}, Unique: true,
	}); err != nil {
		t.Fatal(err)
	}

	// A third NULL is still fine; a real duplicate is not.
	if err := eng.Insert(ctx, "invites", lir.Row{"id": lir.Text("i3")}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "invites", lir.Row{"id": lir.Text("i4"), "email": lir.Text("a@b.co")}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "invites", lir.Row{"id": lir.Text("i5"), "email": lir.Text("a@b.co")}); err == nil {
		t.Fatal("duplicate non-NULL unique value accepted")
	}

	// Updating a NULL into a taken value trips the constraint; updating into
	// NULL never does.
	if _, _, err := eng.Update(ctx, "invites", lir.Row{"id": lir.Text("i1")}, lir.Row{"email": lir.Text("a@b.co")}); err == nil {
		t.Fatal("update into a taken unique value accepted")
	}
	if _, _, err := eng.Update(ctx, "invites", lir.Row{"id": lir.Text("i4")}, lir.Row{"email": lir.Null(catalog.TypeText)}); err != nil {
		t.Fatal(err)
	}
}

// Three-valued logic at the row level: NOT (assignee = "bob") must NOT
// match tasks with a NULL assignee.
func TestExecuteThreeValuedNot(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	d, err := eng.Execute(ctx, many(lir.Project{
		Input: lir.Order{
			Input: qfilter(qscan("tasks", "t"),
				lir.Unary{Op: lir.OpNot, X: qeq(qcol("t", "assignee_id"), qlit("bob"))}),
			Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
		},
		Fields: []lir.ProjField{{As: "id", Expr: qcol("t", "id")}},
	}))
	if err != nil {
		t.Fatal(err)
	}
	want := []any{
		map[string]any{"id": "t3"},
		map[string]any{"id": "t4"},
	} // t2 has NULL assignee: UNKNOWN, filtered; t1 is bob
	if got := jsonish(d); !reflect.DeepEqual(got, want) {
		t.Fatalf("NOT over NULL = %#v, want %#v", got, want)
	}

	// is_null remains the way to ask for NULLs.
	d, err = eng.Execute(ctx, many(lir.Project{
		Input:  qfilter(qscan("tasks", "t"), lir.Unary{Op: lir.OpIsNull, X: qcol("t", "assignee_id")}),
		Fields: []lir.ProjField{{As: "id", Expr: qcol("t", "id")}},
	}))
	if err != nil {
		t.Fatal(err)
	}
	if got := jsonish(d); !reflect.DeepEqual(got, []any{map[string]any{"id": "t2"}}) {
		t.Fatalf("is_null = %#v", got)
	}
}

// Aggregates: grouped folds with the empty-set rules, plus range access.
func TestExecuteAggregatesAndRanges(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	// GROUP BY status over board b1, ordered by the group key.
	d, err := eng.Execute(ctx, many(lir.Order{
		Input: lir.Aggregate{
			Input:  qfilter(qscan("tasks", "t"), qeq(qcol("t", "board_id"), qlit("b1"))),
			Scope:  "stats",
			Groups: []lir.GroupTerm{{Expr: qcol("t", "status")}},
			Terms: []lir.AggTerm{
				{Fn: lir.AggCount, As: "n"},
				{Fn: lir.AggMax, Arg: qcol("t", "priority"), As: "top"},
				{Fn: lir.AggAvg, Arg: qcol("t", "estimate"), As: "avg_est"},
			},
		},
		Terms: []lir.OrderTerm{{Expr: qcol("stats", "status")}},
	}))
	if err != nil {
		t.Fatal(err)
	}
	want := []any{
		map[string]any{"status": "done", "n": int64(1), "top": int64(9), "avg_est": nil},
		map[string]any{"status": "open", "n": int64(2), "top": int64(5), "avg_est": float64(2)},
	}
	if got := jsonish(d); !reflect.DeepEqual(got, want) {
		t.Fatalf("grouped fold = %#v, want %#v", got, want)
	}

	// A global fold over nothing: count 0, everything else NULL — one row.
	d, err = eng.Execute(ctx, many(lir.Aggregate{
		Input: qfilter(qscan("tasks", "t"), qeq(qcol("t", "board_id"), qlit("b3"))),
		Terms: []lir.AggTerm{
			{Fn: lir.AggCount, As: "n"},
			{Fn: lir.AggSum, Arg: qcol("t", "priority"), As: "total"},
		},
	}))
	if err != nil {
		t.Fatal(err)
	}
	if got := jsonish(d); !reflect.DeepEqual(got, []any{map[string]any{"n": int64(0), "total": nil}}) {
		t.Fatalf("empty fold = %#v", got)
	}

	// A range drives the index: board_id = b1 AND status >= "m" rides
	// (board_id, status) and returns only open tasks.
	d, err = eng.Execute(ctx, many(lir.Project{
		Input: lir.Order{
			Input: qfilter(qscan("tasks", "t"),
				qand(qeq(qcol("t", "board_id"), qlit("b1")),
					lir.Binary{Op: lir.OpGte, L: qcol("t", "status"), R: qlit("m")})),
			Terms: []lir.OrderTerm{{Expr: qcol("t", "id")}},
		},
		Fields: []lir.ProjField{{As: "id", Expr: qcol("t", "id")}},
	}))
	if err != nil {
		t.Fatal(err)
	}
	if got := jsonish(d); !reflect.DeepEqual(got, []any{
		map[string]any{"id": "t1"}, map[string]any{"id": "t2"}}) {
		t.Fatalf("range scan = %#v", got)
	}
}

// Ordered-index pushdown returns correctly ordered rows with no sort, and
// slicing stops the scan early.
func TestExecuteOrderedPushdownAndSlice(t *testing.T) {
	eng, ctx, gets, scans := lirSetup(t)

	d, err := eng.Execute(ctx, many(lir.Slice{
		Input: lir.Order{
			Input: qscan("users", "u"),
			Terms: []lir.OrderTerm{{Expr: qcol("u", "name")}},
		},
		Limit: new(int(1)),
	}))
	if err != nil {
		t.Fatal(err)
	}
	got := jsonish(d).([]any)
	if len(got) != 1 || got[0].(map[string]any)["name"] != "Ada" {
		t.Fatalf("pushdown slice = %#v", got)
	}
	// One index scan; one base-row get — the slice stopped before Bob.
	if *scans != 1 || *gets != 1 {
		t.Fatalf("scans=%d gets=%d, want 1 and 1 (stop-early)", *scans, *gets)
	}

	// Root cardinalities.
	one, err := eng.Execute(ctx, lir.Query{Card: lir.CardFirst, Root: lir.Slice{
		Input: qfilter(qscan("users", "u2"), qeq(qcol("u2", "id"), qlit("ada"))),
		Limit: new(int(1)),
	}})
	if err != nil {
		t.Fatal(err)
	}
	if jsonish(one).(map[string]any)["name"] != "Ada" {
		t.Fatalf("first = %#v", jsonish(one))
	}
	scalar, err := eng.Execute(ctx, lir.Query{Card: lir.CardScalar, Root: lir.Aggregate{
		Input: qscan("tasks", "t"),
		Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if jsonish(scalar) != int64(4) {
		t.Fatalf("scalar count = %#v", jsonish(scalar))
	}
	emptyScalar, err := eng.Execute(ctx, lir.Query{Card: lir.CardScalar, Root: lir.Project{
		Input: qfilter(qscan("users", "missing_user"),
			qeq(qcol("missing_user", "id"), qlit("missing"))),
		Fields: []lir.ProjField{{As: "name", Expr: qcol("missing_user", "name")}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if got := jsonish(emptyScalar); got != nil {
		t.Fatalf("empty scalar = %#v, want nil", got)
	}
}
