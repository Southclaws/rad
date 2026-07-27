package planner_test

import (
	"slices"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
	pt "github.com/Southclaws/rad/rad/engine/04_planner/plannertest"
)

func TestCatalogDependenciesFollowObservedColumns(t *testing.T) {
	query := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input: bscan("tasks", "t"),
		Scope: "p",
		Fields: []lir.ProjField{
			{As: "title", Expr: bcol("t", "title")},
		},
	}})
	plan := planner.PlanQuery(query)

	if got := dependencyColumnNames(plan); !slices.Equal(got, []string{"title"}) {
		t.Fatalf("column dependencies = %v, want [title]", got)
	}
	if len(plan.Dependencies.TableExistence) != 1 ||
		plan.Dependencies.TableExistence[0].TableName != "tasks" {
		t.Fatalf("table dependencies = %+v", plan.Dependencies.TableExistence)
	}
	if len(plan.Dependencies.IndexAccess) != 0 {
		t.Fatalf("unselected index dependencies = %+v", plan.Dependencies.IndexAccess)
	}

	scan, ok := plan.Root.(*physical.ProjectExec).Input.(*physical.TableScanExec)
	if !ok {
		t.Fatalf("plan root = %T, want projected table scan", plan.Root)
	}
	if got := columnNames(scan.DecodeColumns); !slices.Equal(got, []string{"title"}) {
		t.Fatalf("decoded columns = %v, want [title]", got)
	}
}

func TestCatalogDependenciesIncludeSelectedIndexAndPredicateColumns(t *testing.T) {
	query := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input: bfilter(bscan("tasks", "t"), band(
			beq(bcol("t", "board_id"), blit("b1")),
			beq(bcol("t", "status"), blit("open")),
		)),
		Scope: "p",
		Fields: []lir.ProjField{
			{As: "title", Expr: bcol("t", "title")},
		},
	}})
	plan := planner.PlanQuery(query)

	if got := dependencyColumnNames(plan); !slices.Equal(got, []string{"board_id", "title", "status"}) {
		t.Fatalf("column dependencies = %v, want [board_id title status]", got)
	}
	if len(plan.Dependencies.IndexAccess) != 1 ||
		plan.Dependencies.IndexAccess[0].IndexName != "tasks_board_status_idx" {
		t.Fatalf("index dependencies = %+v", plan.Dependencies.IndexAccess)
	}

	project := plan.Root.(*physical.ProjectExec)
	filter := project.Input.(*physical.FilterExec)
	scan, ok := filter.Input.(*physical.IndexRangeScanExec)
	if !ok {
		t.Fatalf("access = %T, want index scan", filter.Input)
	}
	if got := columnNames(scan.DecodeColumns); !slices.Equal(got, []string{"board_id", "title", "status"}) {
		t.Fatalf("decoded columns = %v, want [board_id title status]", got)
	}
}

func TestCatalogDependenciesAllowCellFreeRowCounting(t *testing.T) {
	query := bind(t, lir.Query{Card: lir.CardExactlyOne, Root: lir.Aggregate{
		Input: bscan("tasks", "t"),
		Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "count"}},
	}})
	plan := planner.PlanQuery(query)

	if len(plan.Dependencies.TableExistence) != 1 {
		t.Fatalf("table dependencies = %+v", plan.Dependencies.TableExistence)
	}
	if len(plan.Dependencies.ColumnValues) != 0 {
		t.Fatalf("count(*) column dependencies = %+v", plan.Dependencies.ColumnValues)
	}
}

func TestCatalogDependenciesIncludeUnprojectedSortAndTieBreakColumns(t *testing.T) {
	query := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input: lir.Order{
			Input: bscan("tasks", "t"),
			Terms: []lir.OrderTerm{{Expr: bcol("t", "priority")}},
		},
		Scope: "p",
		Fields: []lir.ProjField{
			{As: "title", Expr: bcol("t", "title")},
		},
	}})
	plan := planner.PlanQuery(query)

	if got := dependencyColumnNames(plan); !slices.Equal(got, []string{"id", "title", "priority"}) {
		t.Fatalf("column dependencies = %v, want [id title priority]", got)
	}
	project := plan.Root.(*physical.ProjectExec)
	sort := project.Input.(*physical.SortExec)
	scan := sort.Input.(*physical.TableScanExec)
	if got := columnNames(scan.DecodeColumns); !slices.Equal(got, []string{"id", "title", "priority"}) {
		t.Fatalf("decoded columns = %v, want [id title priority]", got)
	}
}

func TestMutationCatalogDependenciesIncludeTargetWriteProtocol(t *testing.T) {
	catalog, ctx := pt.Catalog(t)
	programBinder, err := binder.NewProgramBinder(ctx, catalog, []string{"create"})
	if err != nil {
		t.Fatal(err)
	}
	statement, err := programBinder.Bind(binder.ProgramStmt{
		Name: "create", Mutation: true, Table: "tasks",
		Rel: lir.Query{Root: lir.Rows{
			Scope: "new",
			Columns: []lir.RowsCol{
				{Name: "id", Kind: lir.KindText},
				{Name: "board_id", Kind: lir.KindText},
				{Name: "title", Kind: lir.KindText},
				{Name: "status", Kind: lir.KindText},
				{Name: "priority", Kind: lir.KindInt64},
			},
			Values: [][]any{{"t-new", "b1", "manifest", "open", 1}},
		}},
	})
	if err != nil {
		t.Fatal(err)
	}

	if statement.Target == nil || statement.Target.Name != "tasks" {
		t.Fatalf("bound target = %+v", statement.Target)
	}
	if len(statement.Plan.Dependencies.WriteProtocols) != 1 ||
		statement.Plan.Dependencies.WriteProtocols[0].TableName != "tasks" {
		t.Fatalf("write protocol dependencies = %+v", statement.Plan.Dependencies.WriteProtocols)
	}
	if got, want := len(statement.Plan.Dependencies.ColumnValues), len(statement.Target.Columns); got != want {
		t.Fatalf("target column dependencies = %d, want %d", got, want)
	}
}

func dependencyColumnNames(plan *physical.PhysPlan) []string {
	names := make([]string, len(plan.Dependencies.ColumnValues))
	for i, dependency := range plan.Dependencies.ColumnValues {
		names[i] = dependency.ColumnName
	}
	return names
}

func columnNames(columns []model.Column) []string {
	names := make([]string, len(columns))
	for i, column := range columns {
		names[i] = column.Name
	}
	return names
}
