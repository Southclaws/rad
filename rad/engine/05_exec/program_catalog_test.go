package exec

import (
	"context"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

func TestTransactionExecutesMultiplePIRProgramsWithoutChangingPIR(t *testing.T) {
	engine, ctx := setup(t)
	tx, err := engine.Begin(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()

	insert := execprogram.Program{Statements: []execprogram.Statement{{
		Name: "insert", Kind: execprogram.Create, Table: "users",
		Rel: lir.Query{Card: lir.CardMany, Root: lir.Rows{
			Scope: "r",
			Columns: []lir.RowsCol{
				{Name: "id", Kind: lir.KindInt64},
				{Name: "name", Kind: lir.KindText},
			},
			Values: [][]any{{int64(77), "inside"}},
		}},
	}}}
	if _, err := tx.ExecuteProgram(ctx, insert, execprogram.Options{}); err != nil {
		t.Fatal(err)
	}

	query := execprogram.Program{Statements: []execprogram.Statement{{
		Name: "read", Kind: execprogram.Query,
		Rel: lir.Query{Card: lir.CardMany, Root: lir.Order{
			Input: lir.Scan{Table: "users", Scope: "u"},
			Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "u", Name: "id"}}},
		}},
	}}}
	result, err := tx.ExecuteProgram(ctx, query, execprogram.Options{})
	if err != nil {
		t.Fatal(err)
	}
	if result.Result.Kind != lir.DatumArray || len(result.Result.Elems) != 1 {
		t.Fatalf("transaction query result = %#v, want own uncommitted row", result.Result)
	}
	if _, ok, err := engine.GetByPrimaryKey(context.Background(), "users", lir.Row{"id": lir.Int64(77)}); err != nil || ok {
		t.Fatalf("uncommitted row leaked: ok=%v err=%v", ok, err)
	}

	if err := tx.Rollback(); err != nil {
		t.Fatal(err)
	}
	if _, ok, err := engine.GetByPrimaryKey(ctx, "users", lir.Row{"id": lir.Int64(77)}); err != nil || ok {
		t.Fatalf("rolled-back row visible: ok=%v err=%v", ok, err)
	}
}

func TestExecuteProgramRejectsStaleExpectedCatalog(t *testing.T) {
	engine, ctx := setup(t)
	expected, err := engine.Catalog().Revision(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := engine.Catalog().CreateTable(ctx, model.TableDef{
		Name: "newer",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}

	_, err = engine.ExecuteProgram(ctx, execprogram.Program{Statements: []execprogram.Statement{{
		Name: "value", Kind: execprogram.Query,
		Rel: lir.Query{Card: lir.CardFirst, Root: lir.Rows{
			Scope: "r", Columns: []lir.RowsCol{{Name: "n", Kind: lir.KindInt64}}, Values: [][]any{{1}},
		}},
	}}}, execprogram.Options{ExpectedCatalog: &expected})
	if !IsConflict(err) {
		t.Fatalf("error = %v, want retryable catalog conflict", err)
	}
}
