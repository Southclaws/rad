package exec

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

func TestHistoricalMissingValueSurvivesDefaultChangesAndReopen(t *testing.T) {
	ctx := t.Context()
	databaseName := t.TempDir()
	var database *kvslate.Store
	var engine *Engine
	open := func() {
		t.Helper()
		database, engine = openFileEngine(
			t,
			databaseName,
			WithSchemaJobScheduling(false),
			withAutomaticReclamation(false),
		)
	}
	close := func() {
		t.Helper()
		if engine != nil {
			if err := engine.Close(); err != nil {
				t.Fatal(err)
			}
			engine = nil
		}
		if database != nil {
			if err := database.Close(); err != nil {
				t.Fatal(err)
			}
			database = nil
		}
	}
	t.Cleanup(close)
	reopen := func() {
		t.Helper()
		close()
		open()
	}
	open()

	if _, err := engine.Catalog().CreateTable(ctx, model.TableDef{
		Name: "items",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	if err := engine.Insert(ctx, "items", lir.Row{"id": lir.Int64(1)}); err != nil {
		t.Fatal(err)
	}
	if _, err := engine.Catalog().CreateColumn(ctx, "items", model.ColumnDef{
		Name: "status", Type: model.TypeText, Nullable: true,
		Default: &model.Default{Text: "active"},
	}); err != nil {
		t.Fatal(err)
	}

	reopen()
	table, status := historicalStatusColumn(t, engine)
	assertStoredColumn(t, engine, table, status, 1, false)
	assertStatus(t, engine, 1, lir.Text("active"))

	if err := engine.Insert(ctx, "items", lir.Row{"id": lir.Int64(2)}); err != nil {
		t.Fatal(err)
	}
	if err := engine.Insert(ctx, "items", lir.Row{
		"id": lir.Int64(3), "status": lir.Null(model.TypeText),
	}); err != nil {
		t.Fatal(err)
	}
	assertStoredColumn(t, engine, table, status, 2, true)
	assertStoredColumn(t, engine, table, status, 3, true)

	if _, err := engine.Catalog().ChangeColumnInsertDefault(
		ctx,
		"items",
		"status",
		&model.Default{Text: "pending"},
	); err != nil {
		t.Fatal(err)
	}
	reopen()
	_, status = historicalStatusColumn(t, engine)
	if status.InsertDefault == nil || status.InsertDefault.Text != "pending" ||
		status.MissingValue == nil || status.MissingValue.Text != "active" {
		t.Fatalf("reopened status semantics = %+v", status)
	}
	assertStatus(t, engine, 1, lir.Text("active"))
	assertStatus(t, engine, 2, lir.Text("active"))
	assertStatus(t, engine, 3, lir.Null(model.TypeText))
	if err := engine.Insert(ctx, "items", lir.Row{"id": lir.Int64(4)}); err != nil {
		t.Fatal(err)
	}
	assertStatus(t, engine, 4, lir.Text("pending"))

	if _, err := engine.Catalog().ChangeColumnInsertDefault(
		ctx,
		"items",
		"status",
		&model.Default{Func: model.DefaultUUID},
	); err != nil {
		t.Fatal(err)
	}
	reopen()
	if err := engine.Insert(ctx, "items", lir.Row{"id": lir.Int64(5)}); err != nil {
		t.Fatal(err)
	}
	generated, ok, err := engine.GetByPrimaryKey(ctx, "items", lir.Row{"id": lir.Int64(5)})
	if err != nil || !ok || !uuidRE.MatchString(generated["status"].Text) {
		t.Fatalf("generated status = %+v found=%v err=%v", generated["status"], ok, err)
	}
	assertStatus(t, engine, 1, lir.Text("active"))

	if _, err := engine.Catalog().ChangeColumnInsertDefault(ctx, "items", "status", nil); err != nil {
		t.Fatal(err)
	}
	reopen()
	table, status = historicalStatusColumn(t, engine)
	if status.InsertDefault != nil || status.MissingValue == nil ||
		status.MissingValue.Text != "active" {
		t.Fatalf("cleared reopened status semantics = %+v", status)
	}
	if err := engine.Insert(ctx, "items", lir.Row{"id": lir.Int64(6)}); err != nil {
		t.Fatal(err)
	}
	assertStatus(t, engine, 1, lir.Text("active"))
	assertStatus(t, engine, 6, lir.Null(model.TypeText))
	assertStoredColumn(t, engine, table, status, 1, false)
	assertStoredColumn(t, engine, table, status, 6, true)
}

func historicalStatusColumn(t *testing.T, engine *Engine) (model.Table, model.Column) {
	t.Helper()
	table, ok, err := engine.Catalog().GetTable(t.Context(), "items")
	if err != nil || !ok {
		t.Fatalf("items table: found=%v err=%v", ok, err)
	}
	status, ok := table.Column("status")
	if !ok {
		t.Fatal("status column is missing")
	}
	return table, status
}

func assertStatus(t *testing.T, engine *Engine, id int64, want lir.Value) {
	t.Helper()
	row, ok, err := engine.GetByPrimaryKey(
		t.Context(),
		"items",
		lir.Row{"id": lir.Int64(id)},
	)
	if err != nil || !ok {
		t.Fatalf("row %d: found=%v err=%v", id, ok, err)
	}
	if got := row["status"]; got.Null != want.Null || !got.Null && !got.Equal(want) {
		t.Fatalf("row %d status = %+v, want %+v", id, got, want)
	}
}

func assertStoredColumn(
	t *testing.T,
	engine *Engine,
	table model.Table,
	column model.Column,
	id int64,
	want bool,
) {
	t.Helper()
	primaryKey, err := codec.EncodeRowTuple(lir.Row{"id": lir.Int64(id)}, table.PrimaryKey)
	if err != nil {
		t.Fatal(err)
	}
	raw, ok, err := engine.store.Get(t.Context(), codec.DataKey(table.ID, primaryKey))
	if err != nil || !ok {
		t.Fatalf("raw row %d: found=%v err=%v", id, ok, err)
	}
	_, found, err := codec.RemoveColumn(raw, column.ID)
	if err != nil {
		t.Fatal(err)
	}
	if found != want {
		t.Fatalf("row %d physical status present=%v, want %v", id, found, want)
	}
}
