package plannertest

import (
	"context"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
)

func Column(scope, name string) lir.Column { return lir.Column{Scope: scope, Name: name} }
func Literal(v any) lir.Literal            { return lir.Literal{Raw: v} }
func Equal(l, r lir.Expr) lir.Expr         { return lir.Binary{Op: lir.OpEq, L: l, R: r} }
func And(l, r lir.Expr) lir.Expr           { return lir.Binary{Op: lir.OpAnd, L: l, R: r} }
func Scan(table, scope string) lir.Scan    { return lir.Scan{Table: table, Scope: scope} }
func Filter(in lir.Relation, pred lir.Expr) lir.Filter {
	return lir.Filter{Input: in, Pred: pred}
}

func Catalog(t *testing.T) (*catalog.Catalog, context.Context) {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)
	create := func(def model.TableDef) {
		if _, err := cat.CreateTable(ctx, def); err != nil {
			t.Fatal(err)
		}
	}
	create(model.TableDef{Name: "users", Columns: []model.ColumnDef{
		{Name: "id", Type: model.TypeText}, {Name: "name", Type: model.TypeText},
	}, PrimaryKey: []string{"id"}, Indexes: []model.IndexDef{{Name: "users_name_uq", Columns: []string{"name"}, Unique: true}}})
	create(model.TableDef{Name: "boards", Columns: []model.ColumnDef{
		{Name: "id", Type: model.TypeText}, {Name: "name", Type: model.TypeText}, {Name: "owner_id", Type: model.TypeText},
	}, PrimaryKey: []string{"id"}, ForeignKeys: []model.ForeignKeyDef{
		{Name: "boards_owner_fk", Columns: []string{"owner_id"}, RefTable: "users", RefColumns: []string{"id"}},
	}})
	create(model.TableDef{Name: "tasks", Columns: []model.ColumnDef{
		{Name: "id", Type: model.TypeText},
		{Name: "board_id", Type: model.TypeText},
		{Name: "title", Type: model.TypeText},
		{Name: "status", Type: model.TypeText},
		{Name: "priority", Type: model.TypeInt64},
		{Name: "estimate", Type: model.TypeFloat64, Nullable: true},
		{Name: "assignee_id", Type: model.TypeText, Nullable: true},
	}, PrimaryKey: []string{"id"}, Indexes: []model.IndexDef{
		{Name: "tasks_board_status_idx", Columns: []string{"board_id", "status"}},
	}, ForeignKeys: []model.ForeignKeyDef{
		{Name: "tasks_board_fk", Columns: []string{"board_id"}, RefTable: "boards", RefColumns: []string{"id"}},
		{Name: "tasks_assignee_fk", Columns: []string{"assignee_id"}, RefTable: "users", RefColumns: []string{"id"}},
	}})
	create(model.TableDef{
		Name: "comments", Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeText}, {Name: "task_id", Type: model.TypeText}, {Name: "body", Type: model.TypeText},
		}, PrimaryKey: []string{"id"}, Indexes: []model.IndexDef{{Name: "comments_task_idx", Columns: []string{"task_id"}}},
		ForeignKeys: []model.ForeignKeyDef{{Name: "comments_task_fk", Columns: []string{"task_id"}, RefTable: "tasks", RefColumns: []string{"id"}}},
	})
	return cat, ctx
}

func Bind(t *testing.T, q lir.Query) *bound.Query {
	t.Helper()
	cat, ctx := Catalog(t)
	wrapped := false
	if q.Card == lir.CardMany {
		if _, ordered := q.Root.(lir.Order); !ordered {
			q.Root = lir.Order{Input: q.Root, Terms: []lir.OrderTerm{{Expr: Literal(true)}}}
			wrapped = true
		}
	}
	bq, err := binder.Bind(ctx, cat, q)
	if err != nil {
		t.Fatal(err)
	}
	if wrapped {
		bq.Root = bq.Root.(*bound.Order).In
	}
	return bq
}

func ForcingQuery() lir.Query {
	owner := Filter(Scan("users", "o"), Equal(Column("o", "id"), Column("b", "owner_id")))
	assignee := Filter(Scan("users", "a"), Equal(Column("a", "id"), Column("t", "assignee_id")))
	commentCount := lir.Aggregate{Input: Filter(Scan("comments", "c"), Equal(Column("c", "task_id"), Column("t", "id"))), Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}}}
	openTasks := lir.Project{
		Input: lir.Slice{Input: lir.Order{
			Input: Filter(Scan("tasks", "t"), And(
				Equal(Column("t", "board_id"), Column("b", "id")), Equal(Column("t", "status"), Literal("open")))),
			Terms: []lir.OrderTerm{{Expr: Column("t", "priority"), Desc: true}},
		}, Limit: new(int(20))},
		Spread: []string{"t"}, Fields: []lir.ProjField{
			{As: "assignee", Expr: lir.First{Rel: assignee}}, {As: "comment_count", Expr: lir.Scalar{Rel: commentCount}},
		},
	}
	return lir.Query{Card: lir.CardMany, Root: lir.Project{Input: Scan("boards", "b"), Spread: []string{"b"}, Fields: []lir.ProjField{
		{As: "owner", Expr: lir.First{Rel: owner}}, {As: "tasks", Expr: lir.Array{Rel: openTasks}},
	}}}
}
