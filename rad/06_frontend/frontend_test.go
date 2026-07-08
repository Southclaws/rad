// These tests document the frontend (06): the public surface of RAD. A
// caller sees a DB handle with DDL, writes, point reads, logical queries,
// and transactions — and nothing else. Everything below (planning, key
// encoding, SlateDB) is reachable only through this API.
//
// Tests run on the in-memory backend; e2e/ replays the same behavior over
// real SlateDB.
package frontend_test

import (
	"context"
	"fmt"
	"testing"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	frontend "rad/rad/06_frontend"
)

// newDB initializes a fresh database with the canonical users/orders schema
// and seed rows used throughout these tests.
func newDB(t *testing.T) (*frontend.DB, context.Context) {
	t.Helper()
	ctx := context.Background()
	db := frontend.Open(memStore(t))

	if _, err := db.CreateTable(ctx, catalog.TableDef{
		Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "name", Type: catalog.TypeText},
			{Name: "age", Type: catalog.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "users_name_idx", Columns: []string{"name"}}},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := db.CreateTable(ctx, catalog.TableDef{
		Name: "orders",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
			{Name: "user_id", Type: catalog.TypeInt64},
			{Name: "total", Type: catalog.TypeFloat64, Nullable: true},
		},
		PrimaryKey:  []string{"id"},
		Indexes:     []catalog.IndexDef{{Name: "orders_user_id_idx", Columns: []string{"user_id"}}},
		ForeignKeys: []catalog.ForeignKeyDef{{Name: "orders_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}}},
	}); err != nil {
		t.Fatal(err)
	}

	seed := []struct {
		table string
		row   lir.Row
	}{
		{"users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Alice"), "age": lir.Int64(30)}},
		{"users", lir.Row{"id": lir.Int64(2), "name": lir.Text("Bob"), "age": lir.Int64(25)}},
		{"users", lir.Row{"id": lir.Int64(3), "name": lir.Text("Carol")}},
		{"orders", lir.Row{"id": lir.Int64(100), "user_id": lir.Int64(1), "total": lir.Float64(9.99)}},
		{"orders", lir.Row{"id": lir.Int64(101), "user_id": lir.Int64(1), "total": lir.Float64(25.00)}},
		{"orders", lir.Row{"id": lir.Int64(102), "user_id": lir.Int64(2), "total": lir.Float64(3.50)}},
	}
	for _, s := range seed {
		if err := db.Insert(ctx, s.table, s.row); err != nil {
			t.Fatal(err)
		}
	}
	return db, ctx
}

// Open attaches to a store; the database is the whole instance — there is
// no schema or database hierarchy to create.
func TestOpen(t *testing.T) {
	ctx := context.Background()
	store := memStore(t)

	db := frontend.Open(store)
	if _, err := db.Tables(ctx); err != nil {
		t.Fatal(err)
	}

	// A second handle over the same store sees the same catalog.
	if _, err := db.CreateTable(ctx, catalog.TableDef{
		Name:       "things",
		Columns:    []catalog.ColumnDef{{Name: "id", Type: catalog.TypeInt64}},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	reopened := frontend.Open(store)
	if _, ok, err := reopened.Catalog().GetTable(ctx, "things"); err != nil || !ok {
		t.Fatalf("reopened handle can't see table: ok=%v err=%v", ok, err)
	}
}

// Insert then Get: the basic write/read cycle. Get takes exactly the primary
// key columns and reports presence with a bool, not an error.
func TestInsertAndGet(t *testing.T) {
	db, ctx := newDB(t)

	row, ok, err := db.Get(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !row["name"].Equal(lir.Text("Alice")) {
		t.Fatalf("got %v", row)
	}

	if _, ok, err := db.Get(ctx, "users", lir.Row{"id": lir.Int64(999)}); err != nil || ok {
		t.Fatalf("missing row: ok=%v err=%v", ok, err)
	}

	// Constraints are enforced on the public surface too.
	if err := db.Insert(ctx, "users", lir.Row{"id": lir.Int64(1), "name": lir.Text("Dup")}); err == nil {
		t.Fatal("duplicate primary key accepted")
	}
	if err := db.Insert(ctx, "orders", lir.Row{"id": lir.Int64(999), "user_id": lir.Int64(42)}); err == nil {
		t.Fatal("dangling foreign key accepted")
	}
}

// Query accepts the logical IR and returns alias-qualified rows. The full
// operator set works through the public API.
func TestQueryOperators(t *testing.T) {
	db, ctx := newDB(t)

	t.Run("scan returns rows in primary key order", func(t *testing.T) {
		rows, err := db.Query(ctx, lir.Query{Root: lir.Scan{Table: "users", Alias: "u"}})
		if err != nil || len(rows) != 3 {
			t.Fatalf("rows=%d err=%v", len(rows), err)
		}
		if !rows[0]["u.id"].Equal(lir.Int64(1)) || !rows[2]["u.id"].Equal(lir.Int64(3)) {
			t.Fatalf("not in pk order: %v", rows)
		}
	})

	t.Run("index scan", func(t *testing.T) {
		rows, err := db.Query(ctx, lir.Query{Root: lir.IndexScan{
			Table: "users", Alias: "u", Index: "users_name_idx",
			Prefix: lir.Row{"name": lir.Text("Bob")},
		}})
		if err != nil || len(rows) != 1 || !rows[0]["u.id"].Equal(lir.Int64(2)) {
			t.Fatalf("rows=%v err=%v", rows, err)
		}
	})

	t.Run("filter and project", func(t *testing.T) {
		rows, err := db.Query(ctx, lir.Query{Root: lir.Project{
			Input: lir.Filter{
				Input: lir.Scan{Table: "orders", Alias: "o"},
				Expr: lir.Eq{
					Left:  lir.ColRef{Alias: "o", Column: "user_id"},
					Right: lir.Literal{Value: lir.Int64(1)},
				},
			},
			Columns: []lir.Projection{{Expr: lir.ColRef{Alias: "o", Column: "id"}, As: "order_id"}},
		}})
		if err != nil || len(rows) != 2 {
			t.Fatalf("rows=%v err=%v", rows, err)
		}
		if len(rows[0]) != 1 {
			t.Fatalf("projection leaked columns: %v", rows[0])
		}
	})

	t.Run("join", func(t *testing.T) {
		rows, err := db.Query(ctx, lir.Query{Root: lir.Join{
			Left:  lir.Scan{Table: "users", Alias: "u"},
			Right: lir.IndexScan{Table: "orders", Alias: "o", Index: "orders_user_id_idx"},
			Type:  lir.InnerJoin,
			On: lir.Eq{
				Left:  lir.ColRef{Alias: "u", Column: "id"},
				Right: lir.ColRef{Alias: "o", Column: "user_id"},
			},
		}})
		if err != nil || len(rows) != 3 {
			t.Fatalf("rows=%d err=%v", len(rows), err)
		}
		for _, r := range rows {
			if !r["u.id"].Equal(r["o.user_id"]) {
				t.Fatalf("join predicate violated: %v", r)
			}
		}
	})

	t.Run("limit", func(t *testing.T) {
		rows, err := db.Query(ctx, lir.Query{Root: lir.Limit{
			Input: lir.Scan{Table: "users", Alias: "u"}, N: 2,
		}})
		if err != nil || len(rows) != 2 {
			t.Fatalf("rows=%d err=%v", len(rows), err)
		}
	})

	t.Run("invalid queries fail at plan time", func(t *testing.T) {
		if _, err := db.Query(ctx, lir.Query{Root: lir.Scan{Table: "ghost", Alias: "g"}}); err == nil {
			t.Fatal("unknown table accepted")
		}
	})
}

// Update and Delete complete CRUD on the public surface, with constraint
// enforcement and delete-restrict.
func TestUpdateAndDelete(t *testing.T) {
	db, ctx := newDB(t)

	if _, found, err := db.Update(ctx, "users", lir.Row{"id": lir.Int64(3)}, lir.Row{"age": lir.Int64(41)}); err != nil || !found {
		t.Fatalf("update: found=%v err=%v", found, err)
	}
	row, _, _ := db.Get(ctx, "users", lir.Row{"id": lir.Int64(3)})
	if !row["age"].Equal(lir.Int64(41)) {
		t.Fatalf("update lost: %v", row)
	}

	// Alice has orders: restrict blocks her deletion.
	if _, err := db.Delete(ctx, "users", lir.Row{"id": lir.Int64(1)}); err == nil {
		t.Fatal("deleted a referenced user")
	}
	// Carol has none: she can go.
	if found, err := db.Delete(ctx, "users", lir.Row{"id": lir.Int64(3)}); err != nil || !found {
		t.Fatalf("delete: found=%v err=%v", found, err)
	}
	if _, ok, _ := db.Get(ctx, "users", lir.Row{"id": lir.Int64(3)}); ok {
		t.Fatal("deleted row still present")
	}
}

// Txn gives multi-statement atomicity with read-your-writes: an order can
// reference a user inserted in the same (uncommitted) transaction, and both
// land together or not at all.
func TestTxnAtomicity(t *testing.T) {
	db, ctx := newDB(t)

	err := db.Txn(ctx, func(tx *frontend.Tx) error {
		if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(10), "name": lir.Text("Dave")}); err != nil {
			return err
		}
		// FK check sees the uncommitted user.
		if err := tx.Insert(ctx, "orders", lir.Row{"id": lir.Int64(200), "user_id": lir.Int64(10)}); err != nil {
			return err
		}
		// So do reads and queries inside the transaction.
		if _, ok, err := tx.Get(ctx, "users", lir.Row{"id": lir.Int64(10)}); err != nil || !ok {
			return fmt.Errorf("read-your-writes failed: ok=%v err=%v", ok, err)
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}

	if _, ok, _ := db.Get(ctx, "orders", lir.Row{"id": lir.Int64(200)}); !ok {
		t.Fatal("committed transactional insert missing")
	}
}

// Returning an error from the transaction function rolls back everything.
func TestTxnRollback(t *testing.T) {
	db, ctx := newDB(t)

	sentinel := fmt.Errorf("changed our mind")
	err := db.Txn(ctx, func(tx *frontend.Tx) error {
		if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(11), "name": lir.Text("Eve")}); err != nil {
			return err
		}
		return sentinel
	})
	if err == nil {
		t.Fatal("fn error not propagated")
	}
	if _, ok, _ := db.Get(ctx, "users", lir.Row{"id": lir.Int64(11)}); ok {
		t.Fatal("rolled-back insert visible")
	}
}

// Transactions read a stable snapshot: writes committed by others after
// Begin are invisible inside, visible after.
func TestTxnSnapshotIsolation(t *testing.T) {
	db, ctx := newDB(t)

	err := db.Txn(ctx, func(tx *frontend.Tx) error {
		// Another writer commits mid-transaction.
		if err := db.Insert(ctx, "users", lir.Row{"id": lir.Int64(12), "name": lir.Text("Zed")}); err != nil {
			return err
		}
		if _, ok, err := tx.Get(ctx, "users", lir.Row{"id": lir.Int64(12)}); err != nil {
			return err
		} else if ok {
			t.Error("snapshot read saw a post-Begin commit")
		}
		return nil
	})
	// This read-only-ish transaction may legitimately conflict at commit
	// (its tracked reads overlap the concurrent insert).
	if err != nil && !frontend.IsConflict(err) {
		t.Fatal(err)
	}

	if _, ok, _ := db.Get(ctx, "users", lir.Row{"id": lir.Int64(12)}); !ok {
		t.Fatal("committed row missing after transaction ended")
	}
}

// Optimistic concurrency: when two transactions race, one loses at commit
// with an error IsConflict recognizes — the standard remedy is to retry.
func TestTxnConflictAndRetry(t *testing.T) {
	db, ctx := newDB(t)

	attempts := 0
	insert13 := func() error {
		return db.Txn(ctx, func(tx *frontend.Tx) error {
			attempts++
			if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(13), "name": lir.Text("Race")}); err != nil {
				return err
			}
			if attempts == 1 {
				// First attempt: a rival commits the same PK before us.
				return db.Insert(ctx, "users", lir.Row{"id": lir.Int64(13), "name": lir.Text("Rival")})
			}
			return nil
		})
	}

	err := insert13()
	if !frontend.IsConflict(err) {
		t.Fatalf("expected conflict, got %v", err)
	}

	// Retry: now the row exists, so the insert fails cleanly (not a
	// conflict) — the retry loop surfaces a real constraint violation.
	err = insert13()
	if err == nil || frontend.IsConflict(err) {
		t.Fatalf("retry: got %v, want duplicate-pk error", err)
	}

	row, ok, _ := db.Get(ctx, "users", lir.Row{"id": lir.Int64(13)})
	if !ok || !row["name"].Equal(lir.Text("Rival")) {
		t.Fatalf("winner's row missing: %v ok=%v", row, ok)
	}
}

// Whole queries run against a transaction's snapshot too — Tx.Query is the
// same logical IR interface as DB.Query.
func TestTxnQuery(t *testing.T) {
	db, ctx := newDB(t)

	err := db.Txn(ctx, func(tx *frontend.Tx) error {
		if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Int64(14), "name": lir.Text("Quinn")}); err != nil {
			return err
		}
		rows, err := tx.Query(ctx, lir.Query{Root: lir.Scan{Table: "users", Alias: "u"}})
		if err != nil {
			return err
		}
		if len(rows) != 4 { // 3 seeded + own uncommitted write
			t.Errorf("tx query saw %d users, want 4", len(rows))
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}
