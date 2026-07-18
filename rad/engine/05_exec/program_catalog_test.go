package exec

import (
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestExecuteProgramRejectsStaleExpectedCatalog(t *testing.T) {
	engine, ctx := setup(t)
	expected, err := engine.Catalog().Schema(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := engine.Catalog().CreateTable(ctx, catalog.TableDef{
		Name: "newer",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeInt64},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}

	_, err = engine.ExecuteProgram(ctx, Program{Statements: []ProgramStatement{{
		Name: "value", Kind: StmtQuery,
		Rel: lir.Query{Card: lir.CardFirst, Root: lir.Rows{
			Scope: "r", Columns: []lir.RowsCol{{Name: "n", Kind: lir.KindInt64}}, Values: [][]any{{1}},
		}},
	}}}, ExecOptions{ExpectedCatalog: &expected})
	if !IsConflict(err) {
		t.Fatalf("error = %v, want retryable catalog conflict", err)
	}
}
