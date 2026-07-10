// Package e2e runs the full stack (frontend/planner/executor over catalog
// and KV) against SlateDB — memory:/// here; file:// and s3:// differ only
// by object-store URL. Building this package needs the native library; run
// via `task test`.
package e2e

import (
	"context"
	"fmt"
	"testing"

	kv "rad/rad/01_kv"
	"rad/rad/01_kv/kvslate"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	exec "rad/rad/05_exec"
)

func backends(t *testing.T) map[string]kv.TransactionalKV {
	t.Helper()
	slate, err := kvslate.Open(fmt.Sprintf("e2e-%s", t.Name()), "memory:///")
	if err != nil {
		t.Fatalf("open slatedb: %v", err)
	}
	t.Cleanup(func() { _ = slate.Close() })
	return map[string]kv.TransactionalKV{
		"slatedb-memory": slate,
	}
}

func TestRelationalStackOverBothBackends(t *testing.T) {
	for name, store := range backends(t) {
		t.Run(name, func(t *testing.T) {
			ctx := context.Background()
			cat := catalog.New(store)
			eng := exec.New(store, cat)

			if _, err := cat.CreateTable(ctx, catalog.TableDef{
				Name: "users",
				Columns: []catalog.ColumnDef{
					{Name: "id", Type: catalog.TypeInt64},
					{Name: "name", Type: catalog.TypeText},
				},
				PrimaryKey: []string{"id"},
				Indexes:    []catalog.IndexDef{{Name: "users_name_idx", Columns: []string{"name"}}},
			}); err != nil {
				t.Fatal(err)
			}
			if _, err := cat.CreateTable(ctx, catalog.TableDef{
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

			for i := int64(1); i <= 3; i++ {
				if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(i), "name": qir.Text(fmt.Sprintf("user%d", i))}); err != nil {
					t.Fatal(err)
				}
			}
			for i, uid := range []int64{1, 1, 2} {
				if err := eng.Insert(ctx, "orders", qir.Row{
					"id": qir.Int64(int64(100 + i)), "user_id": qir.Int64(uid), "total": qir.Float64(float64(i) + 0.5),
				}); err != nil {
					t.Fatal(err)
				}
			}

			// Constraints hold on this backend.
			if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(1), "name": qir.Text("dup")}); err == nil {
				t.Fatal("expected duplicate pk error")
			}
			if err := eng.Insert(ctx, "orders", qir.Row{"id": qir.Int64(999), "user_id": qir.Int64(99)}); err == nil {
				t.Fatal("expected fk violation")
			}

			// A join returns each order with its user; the projection is
			// where the joined row takes its output shape.
			d, err := eng.Execute(ctx, qir.Query{Card: qir.CardMany, Root: qir.Project{
				Input: qir.Join{
					Left:  qir.Scan{Table: "users", Scope: "u"},
					Right: qir.Scan{Table: "orders", Scope: "o"},
					Kind:  qir.InnerJoin,
					On: qir.Binary{Op: qir.OpEq,
						L: qir.Column{Scope: "u", Name: "id"},
						R: qir.Column{Scope: "o", Name: "user_id"},
					},
				},
				Fields: []qir.ProjField{
					{As: "user_id", Expr: qir.Column{Scope: "u", Name: "id"}},
					{As: "order_user", Expr: qir.Column{Scope: "o", Name: "user_id"}},
					{As: "total", Expr: qir.Column{Scope: "o", Name: "total"}},
				},
			}})
			if err != nil {
				t.Fatal(err)
			}
			if len(d.Elems) != 3 {
				t.Fatalf("join returned %d rows, want 3", len(d.Elems))
			}
			for _, el := range d.Elems {
				if el.Fields[0].Datum.Scalar != el.Fields[1].Datum.Scalar {
					t.Fatalf("join predicate violated: %v", el)
				}
			}
		})
	}
}

// TestTransactionsOverBothBackends exercises the transactional write path of
// the full stack on both KV backends: atomic multi-table commit, rollback on
// error, snapshot-consistent queries inside a transaction, and a
// serializable conflict between two racing unique inserts.
func TestTransactionsOverBothBackends(t *testing.T) {
	for name, store := range backends(t) {
		t.Run(name, func(t *testing.T) {
			ctx := context.Background()
			cat := catalog.New(store)
			eng := exec.New(store, cat)

			if _, err := cat.CreateTable(ctx, catalog.TableDef{
				Name: "users",
				Columns: []catalog.ColumnDef{
					{Name: "id", Type: catalog.TypeInt64},
					{Name: "name", Type: catalog.TypeText},
				},
				PrimaryKey: []string{"id"},
				Indexes:    []catalog.IndexDef{{Name: "users_name_uq", Columns: []string{"name"}, Unique: true}},
			}); err != nil {
				t.Fatal(err)
			}
			if _, err := cat.CreateTable(ctx, catalog.TableDef{
				Name: "orders",
				Columns: []catalog.ColumnDef{
					{Name: "id", Type: catalog.TypeInt64},
					{Name: "user_id", Type: catalog.TypeInt64},
				},
				PrimaryKey:  []string{"id"},
				ForeignKeys: []catalog.ForeignKeyDef{{Name: "orders_user_fk", Columns: []string{"user_id"}, RefTable: "users", RefColumns: []string{"id"}}},
			}); err != nil {
				t.Fatal(err)
			}

			// Atomic multi-table insert: the order's FK check sees the user
			// inserted earlier in the same transaction.
			err := eng.Txn(ctx, func(tx *exec.Tx) error {
				if err := tx.Insert(ctx, "users", qir.Row{"id": qir.Int64(1), "name": qir.Text("Alice")}); err != nil {
					return err
				}
				return tx.Insert(ctx, "orders", qir.Row{"id": qir.Int64(100), "user_id": qir.Int64(1)})
			})
			if err != nil {
				t.Fatal(err)
			}

			// Rollback on error: neither row survives.
			err = eng.Txn(ctx, func(tx *exec.Tx) error {
				if err := tx.Insert(ctx, "users", qir.Row{"id": qir.Int64(2), "name": qir.Text("Bob")}); err != nil {
					return err
				}
				return fmt.Errorf("application decided to abort")
			})
			if err == nil {
				t.Fatal("expected fn error to propagate")
			}
			if _, ok, _ := eng.GetByPrimaryKey(ctx, "users", qir.Row{"id": qir.Int64(2)}); ok {
				t.Fatal("rolled-back insert visible")
			}

			// Queries inside a transaction run against its snapshot: a row
			// committed by another writer after Begin is invisible.
			err = eng.Txn(ctx, func(tx *exec.Tx) error {
				if err := eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(3), "name": qir.Text("Carol")}); err != nil {
					return err
				}
				d, err := tx.Execute(ctx, qir.Query{Card: qir.CardMany,
					Root: qir.Scan{Table: "users", Scope: "u"}})
				if err != nil {
					return err
				}
				if len(d.Elems) != 1 {
					t.Fatalf("snapshot query saw %d users, want 1 (Carol committed after Begin)", len(d.Elems))
				}
				return nil
			})
			// The read-only transaction may or may not conflict depending on
			// scan-range overlap with Carol's insert; both outcomes are
			// legitimate here.
			if err != nil && !exec.IsConflict(err) {
				t.Fatal(err)
			}

			// Racing unique inserts: disjoint write sets, conflict comes
			// from the tracked unique-check range scan.
			err = eng.Txn(ctx, func(tx *exec.Tx) error {
				if err := tx.Insert(ctx, "users", qir.Row{"id": qir.Int64(10), "name": qir.Text("dup")}); err != nil {
					return err
				}
				return eng.Insert(ctx, "users", qir.Row{"id": qir.Int64(11), "name": qir.Text("dup")})
			})
			if !exec.IsConflict(err) {
				t.Fatalf("expected serializable conflict, got %v", err)
			}
			rows, err := eng.ScanIndex(ctx, "users", "users_name_uq", qir.Row{"name": qir.Text("dup")})
			if err != nil {
				t.Fatal(err)
			}
			if len(rows) != 1 || !rows[0]["id"].Equal(qir.Int64(11)) {
				t.Fatalf("unique index has %v, want just committed id=11", rows)
			}
		})
	}
}
