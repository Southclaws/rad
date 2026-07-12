package exec

import (
	"context"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func setup(t *testing.T) (*Engine, context.Context) {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)
	eng := New(store, cat)

	_, err = cat.CreateTable(ctx, catalog.TableDef{
		Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "name", Type: catalog.TypeText},
			{Name: "age", Type: catalog.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "users_name_idx", Columns: []string{"name"}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	_, err = cat.CreateTable(ctx, catalog.TableDef{
		Name: "orders",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeInt64},
			{Name: "total", Type: catalog.TypeFloat64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "orders_user_id_idx", Columns: []string{"user_id"}},
		},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "orders_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	return eng, ctx
}

func TestInsertAndGetByPrimaryKey(t *testing.T) {
	eng, ctx := setup(t)

	row := lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice"), "age": lir.Int64(30)}
	if err := eng.Insert(ctx, "users", row); err != nil {
		t.Fatal(err)
	}

	got, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !got["name"].Equal(lir.Text("Alice")) || !got["age"].Equal(lir.Int64(30)) {
		t.Fatalf("got %v", got)
	}

	if _, ok, _ := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(999)}); ok {
		t.Fatal("expected missing row")
	}
}

func TestInsertNullableAndValidation(t *testing.T) {
	eng, ctx := setup(t)

	// Nullable column may be omitted.
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("A")}); err != nil {
		t.Fatal(err)
	}
	got, _, _ := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if !got["age"].Null {
		t.Fatalf("expected stored NULL age, got %v", got["age"])
	}

	// Non-nullable column missing.
	err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(2)})
	if err == nil || !strings.Contains(err.Error(), "not nullable") {
		t.Fatalf("expected not-nullable error, got %v", err)
	}

	// Wrong type.
	err = eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(3), "name": lir.Int64(5)})
	if err == nil || !strings.Contains(err.Error(), "expects text") {
		t.Fatalf("expected type error, got %v", err)
	}

	// Unknown column.
	err = eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(4), "name": lir.Text("D"), "nope": lir.Int64(1)})
	if err == nil || !strings.Contains(err.Error(), "no column") {
		t.Fatalf("expected unknown column error, got %v", err)
	}
}

func TestDuplicatePrimaryKeyRejected(t *testing.T) {
	eng, ctx := setup(t)

	row := lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice")}
	if err := eng.Insert(ctx, "users", row); err != nil {
		t.Fatal(err)
	}
	err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Other")})
	if err == nil || !strings.Contains(err.Error(), "duplicate primary key") {
		t.Fatalf("expected duplicate pk error, got %v", err)
	}
}

func TestCompositePrimaryKey(t *testing.T) {
	eng, ctx := setup(t)
	_, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "line_items",
		Columns: []catalog.ColumnDef{
			{Name: "order_id", Type: catalog.TypeInt64},
			{Name: "line_no", Type: catalog.TypeInt64},
			{Name: "sku", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"order_id", "line_no"},
	})
	if err != nil {
		t.Fatal(err)
	}

	for _, r := range []lir.Row{
		{"order_id": lir.Int64(1), "line_no": lir.Int64(1), "sku": lir.Text("a")},
		{"order_id": lir.Int64(1), "line_no": lir.Int64(2), "sku": lir.Text("b")},
	} {
		if err := eng.Insert(ctx, "line_items", r); err != nil {
			t.Fatal(err)
		}
	}

	got, ok, err := eng.GetByPrimaryKey(ctx, "line_items", lir.Row{"order_id": lir.Int64(1), "line_no": lir.Int64(2)})
	if err != nil || !ok || !got["sku"].Equal(lir.Text("b")) {
		t.Fatalf("got %v ok=%v err=%v", got, ok, err)
	}

	err = eng.Insert(ctx, "line_items", lir.Row{"order_id": lir.Int64(1), "line_no": lir.Int64(2), "sku": lir.Text("dup")})
	if err == nil || !strings.Contains(err.Error(), "duplicate primary key") {
		t.Fatalf("expected duplicate pk error, got %v", err)
	}
}

func TestSecondaryIndexScan(t *testing.T) {
	eng, ctx := setup(t)
	for i, name := range []string{"Alice", "Bob", "Alice"} {
		row := lir.Row{"id": lir.Int64(int64(i + 1)), "name": lir.Text(name)}
		if err := eng.Insert(ctx, "users", row); err != nil {
			t.Fatal(err)
		}
	}

	rows, err := eng.ScanIndex(ctx, "users", "users_name_idx", lir.Row{"name": lir.Text("Alice")})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 2 {
		t.Fatalf("got %d rows, want 2", len(rows))
	}
	for _, r := range rows {
		if !r["name"].Equal(lir.Text("Alice")) {
			t.Fatalf("unexpected row %v", r)
		}
	}

	// Empty prefix scans the whole index.
	rows, err = eng.ScanIndex(ctx, "users", "users_name_idx", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 3 {
		t.Fatalf("got %d rows, want 3", len(rows))
	}
}

func TestUniqueIndexRejection(t *testing.T) {
	eng, ctx := setup(t)
	_, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "accounts",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "email", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "accounts_email_uq", Columns: []string{"email"}, Unique: true},
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	if err := eng.Insert(ctx, "accounts", lir.Row{"id": lir.Int64(1), "email": lir.Text("a@x.com")}); err != nil {
		t.Fatal(err)
	}
	err = eng.Insert(ctx, "accounts", lir.Row{"id": lir.Int64(2), "email": lir.Text("a@x.com")})
	if err == nil || !strings.Contains(err.Error(), "unique index") {
		t.Fatalf("expected unique violation, got %v", err)
	}
	// A different value is fine.
	if err := eng.Insert(ctx, "accounts", lir.Row{"id": lir.Int64(2), "email": lir.Text("b@x.com")}); err != nil {
		t.Fatal(err)
	}
	// Prefix of an existing value must not collide ("a@x" vs "a@x.com").
	if err := eng.Insert(ctx, "accounts", lir.Row{"id": lir.Int64(3), "email": lir.Text("a@x")}); err != nil {
		t.Fatal(err)
	}
}

func TestTxnAtomicMultiInsert(t *testing.T) {
	eng, ctx := setup(t)

	err := eng.Txn(ctx, func(tx *Tx) error {
		if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice")}); err != nil {
			return err
		}
		if err := tx.Insert(ctx, "orders", lir.Row{"id": lir.Int64(100), "user_id": lir.Int64(1)}); err != nil {
			return err
		}
		// The FK parent inserted earlier in this transaction is visible to
		// the order's FK check (read-your-writes), even though nothing has
		// committed yet.
		if _, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)}); err != nil {
			return err
		} else if ok {
			t.Fatal("uncommitted insert visible outside the transaction")
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}

	if _, ok, _ := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)}); !ok {
		t.Fatal("user missing after commit")
	}
	if _, ok, _ := eng.GetByPrimaryKey(ctx, "orders", lir.Row{"id": lir.Int64(100)}); !ok {
		t.Fatal("order missing after commit")
	}
}

func TestTxnErrorRollsBackAllWrites(t *testing.T) {
	eng, ctx := setup(t)

	err := eng.Txn(ctx, func(tx *Tx) error {
		if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice")}); err != nil {
			return err
		}
		// FK violation: no user 99. The user row above must not survive.
		return tx.Insert(ctx, "orders", lir.Row{"id": lir.Int64(100), "user_id": lir.Int64(99)})
	})
	if err == nil || !strings.Contains(err.Error(), "foreign key") {
		t.Fatalf("expected fk violation, got %v", err)
	}
	if _, ok, _ := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)}); ok {
		t.Fatal("write from aborted transaction visible")
	}
}

func TestTxnConcurrentDuplicatePKConflict(t *testing.T) {
	eng, ctx := setup(t)

	err := eng.Txn(ctx, func(tx *Tx) error {
		if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("first")}); err != nil {
			return err
		}
		// A racing transaction inserts the same primary key and commits
		// before we do.
		return eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("racer")})
	})
	if !IsConflict(err) {
		t.Fatalf("expected conflict, got %v", err)
	}

	row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !row["name"].Equal(lir.Text("racer")) {
		t.Fatalf("winner row = %v, want the committed racer", row)
	}
}

// Two transactions insert different primary keys with the same unique-indexed
// value. Their write sets are disjoint (different row and index keys), so
// only the tracked unique-check scan range makes this a conflict — this is
// the phantom case that requires SerializableSnapshot.
func TestTxnConcurrentUniqueInsertConflict(t *testing.T) {
	eng, ctx := setup(t)
	_, err := eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "accounts",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "email", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "accounts_email_uq", Columns: []string{"email"}, Unique: true},
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	err = eng.Txn(ctx, func(tx *Tx) error {
		if err := tx.Insert(ctx, "accounts", lir.Row{"id": lir.Int64(1), "email": lir.Text("a@x.com")}); err != nil {
			return err
		}
		return eng.Insert(ctx, "accounts", lir.Row{"id": lir.Int64(2), "email": lir.Text("a@x.com")})
	})
	if !IsConflict(err) {
		t.Fatalf("expected conflict, got %v", err)
	}

	// Exactly one row committed.
	rows, err := eng.ScanIndex(ctx, "accounts", "accounts_email_uq", lir.Row{"email": lir.Text("a@x.com")})
	if err != nil {
		t.Fatal(err)
	}
	if len(rows) != 1 || !rows[0]["id"].Equal(lir.Int64(2)) {
		t.Fatalf("index rows = %v, want just the committed id=2", rows)
	}
}

func TestForeignKeys(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice")}); err != nil {
		t.Fatal(err)
	}

	// Valid reference.
	err := eng.Insert(ctx, "orders", lir.Row{"id": lir.Int64(10), "user_id": lir.Int64(1), "total": lir.Float64(9.5)})
	if err != nil {
		t.Fatal(err)
	}

	// Dangling reference.
	err = eng.Insert(ctx, "orders", lir.Row{"id": lir.Int64(11), "user_id": lir.Int64(42), "total": lir.Float64(1)})
	if err == nil || !strings.Contains(err.Error(), "foreign key") {
		t.Fatalf("expected fk violation, got %v", err)
	}

	// NULL fk value skips the check (user_id is non-nullable here, so use a
	// dedicated table).
	_, err = eng.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "notes",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "notes_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "notes", lir.Row{"id": lir.Int64(1)}); err != nil {
		t.Fatalf("null fk should be allowed: %v", err)
	}
}

func TestScanTableOrder(t *testing.T) {
	eng, ctx := setup(t)
	for _, id := range []int64{5, 1, 3, -2} {
		row := lir.Row{"id": lir.Int64(id), "name": lir.Text("u")}
		if err := eng.Insert(ctx, "users", row); err != nil {
			t.Fatal(err)
		}
	}

	it, err := eng.ScanTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()

	var ids []int64
	for {
		row, ok, err := it.Next()
		if err != nil {
			t.Fatal(err)
		}
		if !ok {
			break
		}
		ids = append(ids, row["id"].Int64)
	}
	want := []int64{-2, 1, 3, 5}
	if len(ids) != len(want) {
		t.Fatalf("got %v, want %v", ids, want)
	}
	for i := range want {
		if ids[i] != want[i] {
			t.Fatalf("got %v, want %v (primary key order)", ids, want)
		}
	}
}
