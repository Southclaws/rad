package exec

import (
	"context"
	"errors"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestPinnedCatalogTableRenameAllowsAlreadyBoundWriter(t *testing.T) {
	eng, ctx := setup(t)
	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if tx.pinnedCatalogVersion() == 0 {
		t.Fatal("transaction did not pin a catalog generation")
	}
	if err := tx.Insert(ctx, "users", userRow(1, "bound-before-rename", 9)); err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("stable-ID writer conflicted with compatible rename: %v", err)
	}

	row, ok, err := eng.GetByPrimaryKey(ctx, "people", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok || !row["name"].Equal(lir.Text("bound-before-rename")) {
		t.Fatalf("renamed table row = %v ok=%v err=%v", row, ok, err)
	}
	rows, err := eng.ScanIndex(ctx, "people", "people_name_idx", lir.Row{"name": lir.Text("bound-before-rename")})
	if err != nil || len(rows) != 1 {
		t.Fatalf("renamed physical index = %v err=%v", rows, err)
	}
}

func TestPinnedCatalogTableRenameAllowsAlreadyBoundReader(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "reader", 9)); err != nil {
		t.Fatal(err)
	}
	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if err := eng.Catalog().RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	row, ok, err := tx.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok || !row["name"].Equal(lir.Text("reader")) {
		t.Fatalf("pinned old-name read = %v ok=%v err=%v", row, ok, err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("stable-ID reader conflicted with compatible rename: %v", err)
	}
	if _, _, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)}); err == nil {
		t.Fatal("new binding still resolved the retired table name")
	}
}

func TestPinnedCatalogNullableColumnAdditionAllowsOldWriter(t *testing.T) {
	eng, ctx := setup(t)
	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if err := tx.Insert(ctx, "users", userRow(1, "old-shape", 4)); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().CreateColumn(ctx, "users", model.ColumnDef{Name: "nickname", Type: model.TypeText, Nullable: true}); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("old writer conflicted with metadata-only nullable column addition: %v", err)
	}
	row, ok, err := eng.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(1)})
	if err != nil || !ok {
		t.Fatalf("new-shape read: ok=%v err=%v", ok, err)
	}
	if nickname, ok := row["nickname"]; !ok || !nickname.Null {
		t.Fatalf("historically missing nullable value = %v, present=%v", nickname, ok)
	}
}

func TestPinnedCatalogColumnRenameUsesStablePhysicalIndexColumns(t *testing.T) {
	eng, ctx := setup(t)
	transition, err := eng.startIndexBuild(ctx, "users", model.IndexDef{
		Name: "users_name_online", Columns: []string{"name"},
	})
	if err != nil {
		t.Fatal(err)
	}
	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if err := tx.Insert(ctx, "users", userRow(1, "old-name-writer", 5)); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().RenameColumn(ctx, "users", "name", "full_name"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("stable-column-ID writer conflicted with rename: %v", err)
	}
	if err := eng.Insert(ctx, "users", lir.Row{"id": lir.Int64(2), "full_name": lir.Text("new-name-writer"), "age": lir.Int64(6)}); err != nil {
		t.Fatal(err)
	}

	ready, err := eng.runIndexBuild(ctx, transition.ID, 1)
	if err != nil {
		t.Fatal(err)
	}
	if ready.State != model.TransitionReady {
		t.Fatalf("transition = %+v", ready)
	}
	assertIndexEqualsTable(t, ctx, eng, "users", "users_name_online")
	rows, err := eng.ScanIndex(ctx, "users", "users_full_name_idx", lir.Row{"full_name": lir.Text("old-name-writer")})
	if err != nil || len(rows) != 1 || !rows[0]["id"].Equal(lir.Int64(1)) {
		t.Fatalf("ready index after column rename = %v err=%v", rows, err)
	}
}

func TestPinnedCatalogColumnDeleteConflictsPinnedRowDecoder(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
			{Name: "retiring", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if err := tx.Insert(ctx, "scratch", lir.Row{"id": lir.Int64(1), "value": lir.Text("old")}); err != nil {
		t.Fatal(err)
	}
	if _, err := eng.Catalog().DeleteColumn(ctx, "scratch", "retiring"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("pinned full-row writer crossed a column value-definition change: %v", err)
	}
}

func TestPinnedCatalogTableDeleteConflictsDependentWriter(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	if err := tx.Insert(ctx, "scratch", lir.Row{"id": lir.Int64(1), "value": lir.Text("doomed")}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Catalog().DeleteTable(ctx, "scratch"); err != nil {
		t.Fatal(err)
	}
	if err := tx.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("writer crossed logical table deletion: %v", err)
	}
}

func TestPinnedProjectedReaderAllowsUnobservedColumnDelete(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
			{Name: "retiring", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "scratch", lir.Row{
		"id": lir.Int64(1), "value": lir.Text("kept"), "retiring": lir.Text("gone"),
	}); err != nil {
		t.Fatal(err)
	}

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	if _, err := eng.Catalog().DeleteColumn(ctx, "scratch", "retiring"); err != nil {
		t.Fatal(err)
	}
	result, err := tx.Execute(ctx, projectedScratchColumn("value"))
	if err != nil {
		t.Fatalf("projected read after unrelated column deletion: %v", err)
	}
	if result.Kind != lir.DatumArray || len(result.Elems) != 1 {
		t.Fatalf("projected result = %+v", result)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("unobserved column deletion conflicted with projected reader: %v", err)
	}
}

func TestPinnedProjectedReaderConflictsWithObservedColumnDelete(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "scratch", lir.Row{
		"id": lir.Int64(1), "value": lir.Text("observed"),
	}); err != nil {
		t.Fatal(err)
	}

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	if _, err := eng.Catalog().DeleteColumn(ctx, "scratch", "value"); err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute(ctx, projectedScratchColumn("value")); err != nil {
		t.Fatalf("old snapshot could not execute its pinned plan: %v", err)
	}
	if err := tx.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("observed column deletion did not conflict with projected reader: %v", err)
	}
}

func TestPinnedProjectedReaderConflictsWithFilteredColumnDelete(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
			{Name: "filtering", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "scratch", lir.Row{
		"id": lir.Int64(1), "value": lir.Text("observed"), "filtering": lir.Text("match"),
	}); err != nil {
		t.Fatal(err)
	}

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	if _, err := eng.Catalog().DeleteColumn(ctx, "scratch", "filtering"); err != nil {
		t.Fatal(err)
	}
	query := projectedScratchColumn("value")
	query.Root = lir.Project{
		Input: lir.Order{
			Input: lir.Filter{
				Input: lir.Scan{Table: "scratch", Scope: "s"},
				Pred: lir.Binary{
					Op: lir.OpEq,
					L:  lir.Column{Scope: "s", Name: "filtering"},
					R:  lir.Literal{Raw: "match"},
				},
			},
			Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "s", Name: "id"}}},
		},
		Fields: []lir.ProjField{{
			As: "value", Expr: lir.Column{Scope: "s", Name: "value"},
		}},
	}
	if _, err := tx.Execute(ctx, query); err != nil {
		t.Fatalf("old snapshot could not execute its pinned filter: %v", err)
	}
	if err := tx.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("filtered column deletion did not conflict with projected reader: %v", err)
	}
}

func TestPinnedCountReaderAllowsColumnDelete(t *testing.T) {
	eng, ctx := setup(t)
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "retiring", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := eng.Insert(ctx, "scratch", lir.Row{
		"id": lir.Int64(1), "retiring": lir.Text("gone"),
	}); err != nil {
		t.Fatal(err)
	}

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	if _, err := eng.Catalog().DeleteColumn(ctx, "scratch", "retiring"); err != nil {
		t.Fatal(err)
	}
	result, err := tx.Execute(ctx, lir.Query{Card: lir.CardExactlyOne, Root: lir.Aggregate{
		Input: lir.Scan{Table: "scratch", Scope: "s"},
		Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "count"}},
	}})
	if err != nil {
		t.Fatalf("count after column deletion: %v", err)
	}
	if result.Kind != lir.DatumObject || len(result.Fields) != 1 ||
		result.Fields[0].Name != "count" ||
		!result.Fields[0].Datum.Scalar.Equal(lir.Int64(1)) {
		t.Fatalf("count result = %+v", result)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("column deletion conflicted with cell-free count: %v", err)
	}
}

func TestPinnedIndexReaderConflictsWithSelectedIndexDelete(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "indexed", 9)); err != nil {
		t.Fatal(err)
	}

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	if err := eng.Catalog().DeleteIndex(ctx, "users", "users_name_idx"); err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute(ctx, usersProjection(lir.Filter{
		Input: lir.Scan{Table: "users", Scope: "u"},
		Pred: lir.Binary{
			Op: lir.OpEq,
			L:  lir.Column{Scope: "u", Name: "name"},
			R:  lir.Literal{Raw: "indexed"},
		},
	})); err != nil {
		t.Fatalf("old snapshot could not execute its pinned index plan: %v", err)
	}
	if err := tx.Commit(ctx); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("selected index deletion did not conflict with reader: %v", err)
	}
}

func TestPinnedTableReaderAllowsUnselectedIndexDelete(t *testing.T) {
	eng, ctx := setup(t)
	if err := eng.Insert(ctx, "users", userRow(1, "table-scan", 9)); err != nil {
		t.Fatal(err)
	}

	tx, err := eng.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	if err := eng.Catalog().DeleteIndex(ctx, "users", "users_name_idx"); err != nil {
		t.Fatal(err)
	}
	if _, err := tx.Execute(ctx, usersProjection(lir.Scan{Table: "users", Scope: "u"})); err != nil {
		t.Fatalf("table scan after unselected index deletion: %v", err)
	}
	if err := tx.Commit(ctx); err != nil {
		t.Fatalf("unselected index deletion conflicted with reader: %v", err)
	}
}

func TestDependencyAdmissionRejectsColumnDeletedBetweenCatalogPinAndDataSnapshot(t *testing.T) {
	pinned := make(chan struct{})
	resume := make(chan struct{})
	eng, ctx := setupWithOptions(t, WithYieldHook(func(ctx context.Context, event YieldEvent) {
		if event.Actor != "reader" || event.Point != YieldCatalogPinned {
			return
		}
		close(pinned)
		select {
		case <-resume:
		case <-ctx.Done():
		}
	}))
	if _, err := eng.Catalog().CreateTable(ctx, model.TableDef{
		Name: "scratch",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}

	type beginResult struct {
		tx  *Tx
		err error
	}
	begun := make(chan beginResult, 1)
	go func() {
		tx, err := eng.Begin(WithYieldActor(ctx, "reader"))
		begun <- beginResult{tx: tx, err: err}
	}()

	<-pinned
	if _, err := eng.Catalog().DeleteColumn(ctx, "scratch", "value"); err != nil {
		t.Fatal(err)
	}
	close(resume)

	result := <-begun
	if result.err != nil {
		t.Fatal(result.err)
	}
	defer result.tx.Rollback()
	if _, err := result.tx.Execute(ctx, projectedScratchColumn("value")); !errors.Is(err, kv.ErrConflict) {
		t.Fatalf("stale dependency admission = %v, want conflict before execution", err)
	}
}

func TestTransactionCatalogSnapshotMatchesCatalogPinnedBeforeDataSnapshot(t *testing.T) {
	pinned := make(chan struct{})
	resume := make(chan struct{})
	eng, ctx := setupWithOptions(t, WithYieldHook(func(ctx context.Context, event YieldEvent) {
		if event.Actor != "writer" || event.Point != YieldCatalogPinned {
			return
		}
		close(pinned)
		select {
		case <-resume:
		case <-ctx.Done():
		}
	}))

	type beginResult struct {
		tx  *Tx
		err error
	}
	begun := make(chan beginResult, 1)
	go func() {
		tx, err := eng.Begin(WithYieldActor(ctx, "writer"))
		begun <- beginResult{tx: tx, err: err}
	}()

	<-pinned
	if err := eng.Catalog().RenameTable(ctx, "users", "people"); err != nil {
		t.Fatal(err)
	}
	close(resume)

	result := <-begun
	if result.err != nil {
		t.Fatal(result.err)
	}
	defer result.tx.Rollback()
	revision, tables, err := result.tx.CatalogSnapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if revision.Version != result.tx.pinnedCatalogVersion() {
		t.Fatalf("catalog snapshot generation = %d, pinned = %d", revision.Version, result.tx.pinnedCatalogVersion())
	}
	foundOld, foundNew := false, false
	for _, table := range tables {
		foundOld = foundOld || table.Name == "users"
		foundNew = foundNew || table.Name == "people"
	}
	if !foundOld || foundNew {
		t.Fatalf("transaction catalog snapshot names: old=%v new=%v tables=%+v", foundOld, foundNew, tables)
	}
	if err := result.tx.Insert(ctx, "users", userRow(77, "pinned-name", 7)); err != nil {
		t.Fatal(err)
	}
	if err := result.tx.Commit(ctx); err != nil {
		t.Fatalf("commit after compatible rename = %v", err)
	}
	row, ok, err := eng.GetByPrimaryKey(ctx, "people", lir.Row{"id": lir.Int64(77)})
	if err != nil || !ok || !row["name"].Equal(lir.Text("pinned-name")) {
		t.Fatalf("renamed table row = %v ok=%v err=%v", row, ok, err)
	}
}

func projectedScratchColumn(column string) lir.Query {
	return lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input: lir.Order{
			Input: lir.Scan{Table: "scratch", Scope: "s"},
			Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "s", Name: "id"}}},
		},
		Fields: []lir.ProjField{{
			As: column, Expr: lir.Column{Scope: "s", Name: column},
		}},
	}}
}

func usersProjection(input lir.Relation) lir.Query {
	return lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: lir.Project{
			Input: input,
			Scope: "p",
			Fields: []lir.ProjField{
				{As: "id", Expr: lir.Column{Scope: "u", Name: "id"}},
				{As: "name", Expr: lir.Column{Scope: "u", Name: "name"}},
			},
		},
		Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "p", Name: "id"}}},
	}}
}
