package exec

// Every statement binds and executes against one KV view. The observable
// half of that contract: schema lookups made inside a transaction join its
// read set, so a schema change committed while the transaction is open
// conflicts at commit instead of passing unseen beneath a stale binding.

import (
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestConcurrentSchemaChangeConflictsWithOpenTxn(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	// The statement resolves "users" through the transaction's snapshot.
	if _, err := tx.Execute(ctx, lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: lir.Scan{Table: "users", Scope: "u"},
		Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "u", Name: "id"}}},
	}}); err != nil {
		t.Fatal(err)
	}
	if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Text("eve"), "name": lir.Text("Eve")}); err != nil {
		t.Fatal(err)
	}

	// A schema change on the same table commits while the transaction is open.
	if _, err := eng.Catalog().CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "bio", Type: catalog.TypeText, Nullable: true,
	}); err != nil {
		t.Fatal(err)
	}

	err = tx.Commit(ctx)
	if !IsConflict(err) {
		t.Fatalf("commit after concurrent schema change = %v, want a conflict", err)
	}
}

// A transaction that never touched the altered table is unaffected.
func TestConcurrentSchemaChangeOnOtherTableDoesNotConflict(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	// Touches boards' schema and users' data (the FK parent row) — but not
	// users' schema, which is all the concurrent schema change writes.
	if err := tx.Insert(ctx, "boards", lir.Row{
		"id": lir.Text("b-snap"), "name": lir.Text("Snap"), "owner_id": lir.Text("ada"),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().CreateColumn(ctx, "users", catalog.ColumnDef{
		Name: "bio", Type: catalog.TypeText, Nullable: true,
	}); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("commit = %v, want success", err)
	}
}

// Engine.Execute reads from one snapshot: a scan iterator opened by the
// statement never observes rows committed after the statement began. The
// per-statement snapshot is not directly injectable, so this exercises the
// public surface: ScanTable's snapshot survives a concurrent insert.
func TestEngineScanHoldsOneSnapshot(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	it, err := eng.ScanTable(ctx, "users")
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()

	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Text("zed"), "name": lir.Text("Zed")}); err != nil {
		t.Fatal(err)
	}

	n := 0
	for {
		row, ok, err := it.Next()
		if err != nil {
			t.Fatal(err)
		}
		if !ok {
			break
		}
		if row["id"].Text == "zed" {
			t.Fatal("iterator observed a row committed after the snapshot")
		}
		n++
	}
	if n == 0 {
		t.Fatal("scan returned no rows")
	}
}
