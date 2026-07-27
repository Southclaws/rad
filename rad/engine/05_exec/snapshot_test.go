package exec

// Transactions carry immutable catalog definitions and admit only their typed
// compatibility fences into the data transaction. Compatible publications can
// therefore overlap; incompatible value/existence/write-protocol changes still
// conflict at commit.

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestConcurrentNullableColumnAdditionDoesNotConflictWithOpenTxn(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	// The statement resolves "users" through the transaction's pinned catalog.
	if _, err := tx.Execute(ctx, lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: lir.Scan{Table: "users", Scope: "u"},
		Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "u", Name: "id"}}},
	}}); err != nil {
		t.Fatal(err)
	}
	if err := tx.Insert(ctx, "users", lir.Row{"id": lir.Text("eve"), "name": lir.Text("Eve")}); err != nil {
		t.Fatal(err)
	}

	// A nullable addition is metadata-only and does not invalidate the pinned
	// row definition or write protocol.
	if _, err := eng.Catalog().CreateColumn(ctx, "users", model.ColumnDef{
		Name: "bio", Type: model.TypeText, Nullable: true,
	}); err != nil {
		t.Fatal(err)
	}

	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("commit after compatible schema change = %v, want success", err)
	}
	row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Text("eve")})
	if err != nil || !ok {
		t.Fatalf("committed row: ok=%v err=%v", ok, err)
	}
	if bio, ok := row["bio"]; !ok || !bio.Null {
		t.Fatalf("historically missing bio = %v present=%v", bio, ok)
	}
}

func TestInsertDefaultPublicationFencesOldWritersButNotReaders(t *testing.T) {
	eng, ctx, _, _ := lirSetup(t)
	if err := eng.Insert(ctx, "users", lir.Row{
		"id": lir.Text("historical"), "name": lir.Text("Historical"),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().CreateColumn(ctx, "users", model.ColumnDef{
		Name: "status", Type: model.TypeText, Nullable: true,
		Default: &model.Default{Text: "active"},
	}); err != nil {
		t.Fatal(err)
	}

	reader, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Rollback()
	row, ok, err := reader.GetByPrimaryKey(
		ctx,
		"users",
		lir.Row{"id": lir.Text("historical")},
	)
	if err != nil || !ok || !row["status"].Equal(lir.Text("active")) {
		t.Fatalf("reader saw row=%v found=%v err=%v", row, ok, err)
	}

	writer, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer writer.Rollback()
	staged, err := writer.Create(ctx, "users", lir.Row{
		"id": lir.Text("racing"), "name": lir.Text("Racing"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if !staged["status"].Equal(lir.Text("active")) {
		t.Fatalf("old writer staged status = %+v", staged["status"])
	}

	if _, err := eng.Catalog().ChangeColumnInsertDefault(
		ctx,
		"users",
		"status",
		&model.Default{Text: "pending"},
	); err != nil {
		t.Fatal(err)
	}
	if err := reader.Commit(ctx); err != nil {
		t.Fatalf("reader conflicted with write-policy-only publication: %v", err)
	}
	if err := writer.Commit(ctx); !IsConflict(err) {
		t.Fatalf("old writer commit = %v, want retryable conflict", err)
	}
	if _, ok, err := eng.GetByPrimaryKey(
		ctx,
		"users",
		lir.Row{"id": lir.Text("racing")},
	); err != nil || ok {
		t.Fatalf("conflicted writer leaked row: found=%v err=%v", ok, err)
	}

	retried, err := eng.Create(ctx, "users", lir.Row{
		"id": lir.Text("racing"), "name": lir.Text("Racing"),
	})
	if err != nil {
		t.Fatal(err)
	}
	if !retried["status"].Equal(lir.Text("pending")) {
		t.Fatalf("retried writer status = %+v, want pending", retried["status"])
	}
	historical, ok, err := eng.GetByPrimaryKey(
		ctx,
		"users",
		lir.Row{"id": lir.Text("historical")},
	)
	if err != nil || !ok || !historical["status"].Equal(lir.Text("active")) {
		t.Fatalf("historical value changed: row=%v found=%v err=%v", historical, ok, err)
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
	if _, err := eng.Catalog().CreateColumn(ctx, "users", model.ColumnDef{
		Name: "bio", Type: model.TypeText, Nullable: true,
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
