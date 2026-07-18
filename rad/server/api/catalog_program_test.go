package api

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestExecuteCatalogProgramCreatesSchemaThenUsesIt(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, columnID := pirwire.SchemaID(1), pirwire.SchemaID(1)

	program := pirwire.Prog("all",
		pirwire.CreateTable("schema", pirwire.TableDefinition{
			ID: &tableID, Name: "notes",
			Columns:    []pirwire.ColumnDefinition{{ID: &columnID, Name: "id", Type: pirwire.ColumnTypeInt64}},
			PrimaryKey: []string{"id"},
		}),
		pirwire.Create("seed", "notes", relBytes(rowsRel("r",
			tcol("id", "int64"), [][]lirwire.Cell{{mustValue(1)}, {mustValue(2)}}))),
		pirwire.Query("all", relBytes(scanOrdered("notes", "n", "id"))),
	)
	result, err := c.Execute(ctx, program)
	if err != nil {
		t.Fatal(err)
	}
	rows, ok := result.Result.([]any)
	if !ok || len(rows) != 2 {
		t.Fatalf("result = %#v, want two rows", result.Result)
	}
	if len(result.Statements) != 3 || result.Statements[0].Affected != 1 || result.Statements[1].Affected != 2 {
		t.Fatalf("summary = %#v", result.Statements)
	}
	info, err := c.Info(ctx)
	if err != nil || info.SchemaVersion != 1 {
		t.Fatalf("info = %+v, %v", info, err)
	}
}

func TestExecuteCatalogProgramUsesOneRevisionPerDirectStatement(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, bodyColumn := pirwire.SchemaID(7), pirwire.SchemaID(1), pirwire.SchemaID(2)

	result, err := c.Execute(ctx, pirwire.Prog("",
		pirwire.CreateTable("table", pirwire.TableDefinition{
			ID: &tableID, Name: "notes",
			Columns:    []pirwire.ColumnDefinition{{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64}},
			PrimaryKey: []string{"id"},
		}),
		pirwire.CreateColumn("body", tableID, pirwire.ColumnDefinition{
			ID: &bodyColumn, Name: "body", Type: pirwire.ColumnTypeText, Nullable: ptrBool(true),
		}),
	))
	if err != nil {
		t.Fatal(err)
	}
	if result.Result != nil || len(result.Statements) != 2 {
		t.Fatalf("catalog-only result = %#v", result)
	}
	info, err := c.Info(ctx)
	if err != nil || info.SchemaVersion != 2 {
		t.Fatalf("info = %+v, %v", info, err)
	}
}

func TestExecuteCatalogProgramRenameIsVisibleToLaterQuery(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, columnID := pirwire.SchemaID(1), pirwire.SchemaID(1)
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("create", pirwire.TableDefinition{
		ID: &tableID, Name: "users",
		Columns:    []pirwire.ColumnDefinition{{ID: &columnID, Name: "id", Type: pirwire.ColumnTypeInt64}},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}

	result, err := c.Execute(ctx, pirwire.Prog("accounts",
		pirwire.RenameTable("rename", tableID, "accounts"),
		pirwire.Query("accounts", relBytes(scanOrdered("accounts", "a", "id"))),
	))
	if err != nil {
		t.Fatal(err)
	}
	if rows, ok := result.Result.([]any); !ok || len(rows) != 0 {
		t.Fatalf("result = %#v, want empty rows", result.Result)
	}
	if tables, err := c.Tables(ctx); err != nil || len(tables) != 1 || tables[0].Name != "accounts" {
		t.Fatalf("tables = %#v, %v", tables, err)
	}
}

func TestExecuteCatalogProgramRollsBackSchemaAndRevision(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, columnID := pirwire.SchemaID(1), pirwire.SchemaID(1)

	_, err := c.Execute(ctx, pirwire.Prog("bad",
		pirwire.CreateTable("schema", pirwire.TableDefinition{
			ID: &tableID, Name: "notes",
			Columns:    []pirwire.ColumnDefinition{{ID: &columnID, Name: "id", Type: pirwire.ColumnTypeInt64}},
			PrimaryKey: []string{"id"},
		}),
		pirwire.Query("bad", relBytes(scanOrdered("missing", "m", "id"))),
	))
	if err == nil {
		t.Fatal("program should fail while binding the query")
	}
	tables, tableErr := c.Tables(ctx)
	info, infoErr := c.Info(ctx)
	if tableErr != nil || infoErr != nil || len(tables) != 0 || info.SchemaVersion != 0 {
		t.Fatalf("rollback left tables=%#v info=%+v errors=(%v, %v)", tables, info, tableErr, infoErr)
	}
}

func TestExecuteCatalogIndexBackfillSeesEarlierProgramWrites(t *testing.T) {
	c := testServerInMode(t, model.ModeDirect)
	ctx := context.Background()
	tableID, idColumn, valueColumn := pirwire.SchemaID(1), pirwire.SchemaID(1), pirwire.SchemaID(2)
	if _, err := c.Execute(ctx, pirwire.Prog("", pirwire.CreateTable("schema", pirwire.TableDefinition{
		ID: &tableID, Name: "items",
		Columns: []pirwire.ColumnDefinition{
			{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
			{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
		},
		PrimaryKey: []string{"id"},
	}))); err != nil {
		t.Fatal(err)
	}

	_, err := c.Execute(ctx, pirwire.Prog("seed",
		pirwire.Create("seed", "items", relBytes(rowsRel("r",
			tcol("id", "int64", "value", "text"),
			[][]lirwire.Cell{{mustValue(1), mustValue("same")}, {mustValue(2), mustValue("same")}}))),
		pirwire.CreateIndex("unique_value", tableID, pirwire.IndexDefinition{
			Name: "items_value_key", Columns: []string{"value"}, Unique: ptrBool(true),
		}),
	))
	if err == nil {
		t.Fatal("unique-index backfill should reject preceding duplicate writes")
	}
	rows, queryErr := c.Query(ctx, scanOrdered("items", "i", "id"))
	tables, tableErr := c.Tables(ctx)
	info, infoErr := c.Info(ctx)
	if queryErr != nil || tableErr != nil || infoErr != nil || len(rows) != 0 ||
		len(tables) != 1 || len(tables[0].Indexes) != 0 || info.SchemaVersion != 1 {
		t.Fatalf("rollback left rows=%#v tables=%#v info=%+v errors=(%v, %v, %v)",
			rows, tables, info, queryErr, tableErr, infoErr)
	}
}

func TestPIRLiteralDefaultKeepsFullInt64Precision(t *testing.T) {
	definition := pirwire.ColumnDefinition{
		Name: "counter", Type: pirwire.ColumnTypeInt64,
		Default: &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{
			Kind: "literal", Value: json.RawMessage(`9007199254740993`),
		}},
	}
	converted, err := pirColumnDef(definition)
	if err != nil {
		t.Fatal(err)
	}
	if converted.Default == nil || converted.Default.Int64 != 9007199254740993 {
		t.Fatalf("default = %+v", converted.Default)
	}
}
