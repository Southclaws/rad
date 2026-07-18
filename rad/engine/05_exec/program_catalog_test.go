package exec

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

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
