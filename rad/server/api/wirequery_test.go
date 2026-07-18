package api

// WireQuery must be a faithful inverse of lowerQuery: a query raised to wire,
// marshalled, decoded, and lowered back must execute to the same result as the
// original. Executing both (rather than comparing structurally) sidesteps
// benign representation differences like an int literal decoding as int64.

import (
	"context"
	"reflect"
	"testing"

	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/engine/06_frontend/resultjson"
	"github.com/Southclaws/rad/rad/protocol"
)

func TestWireQueryRoundTrip(t *testing.T) {
	ctx := context.Background()
	store, err := kvslate.Open("wireq-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)
	db := frontend.Open(store)

	mustTable(t, ctx, cat, model.TableDef{
		Name: "t", PrimaryKey: []string{"id"},
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeText},
			{Name: "n", Type: model.TypeInt64, Nullable: true},
		},
	})
	mustTable(t, ctx, cat, model.TableDef{
		Name: "u", PrimaryKey: []string{"id"},
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeText},
			{Name: "t_id", Type: model.TypeText, Nullable: true},
		},
		ForeignKeys: []model.ForeignKeyDef{{Name: "u_fk", Columns: []string{"t_id"}, RefTable: "t", RefColumns: []string{"id"}}},
	})
	for i, n := range []int64{1, 2, 3} {
		mustInsert(t, ctx, db, "t", lir.Row{"id": lir.Text(idOf("t", i)), "n": lir.Int64(n)})
	}
	mustInsert(t, ctx, db, "t", lir.Row{"id": lir.Text("t3"), "n": lir.Null(model.TypeInt64)})
	for i := range 3 {
		mustInsert(t, ctx, db, "u", lir.Row{"id": lir.Text(idOf("u", i)), "t_id": lir.Text(idOf("t", i))})
	}

	col := func(s, c string) lir.Expr { return lir.Column{Scope: s, Name: c} }
	order := func(in lir.Relation, e lir.Expr) lir.Query {
		return lir.Query{Card: lir.CardMany, Root: lir.Order{Input: in, Terms: []lir.OrderTerm{{Expr: e}}}}
	}

	cases := map[string]lir.Query{
		"scan": order(lir.Scan{Table: "t", Scope: "t"}, col("t", "id")),
		"filter": order(lir.Filter{
			Input: lir.Scan{Table: "t", Scope: "t"},
			Pred:  lir.Binary{Op: lir.OpGt, L: col("t", "n"), R: lir.Literal{Raw: int64(1)}},
		}, col("t", "id")),
		"join": order(lir.Project{
			Input: lir.Join{
				Left: lir.Scan{Table: "u", Scope: "u"}, Right: lir.Scan{Table: "t", Scope: "t"},
				Kind: lir.LeftJoin, On: lir.Binary{Op: lir.OpEq, L: col("u", "t_id"), R: col("t", "id")},
			},
			Scope:  "j",
			Fields: []lir.ProjField{{As: "uid", Expr: col("u", "id")}, {As: "tn", Expr: col("t", "n")}},
		}, col("j", "uid")),
		"aggregate": order(lir.Aggregate{
			Input: lir.Scan{Table: "t", Scope: "t"}, Scope: "g",
			Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "c"}, {Fn: lir.AggSum, Arg: col("t", "n"), As: "s"}},
		},
			col("g", "c")),
		"crossing": order(lir.Project{
			Input: lir.Scan{Table: "t", Scope: "t"}, Scope: "p",
			Fields: []lir.ProjField{
				{As: "id", Expr: col("t", "id")},
				{As: "has", Expr: lir.Exists{Rel: lir.Filter{
					Input: lir.Scan{Table: "u", Scope: "u"},
					Pred:  lir.Binary{Op: lir.OpEq, L: col("u", "t_id"), R: col("t", "id")},
				}}},
			},
		}, col("p", "id")),
	}

	for name, q := range cases {
		t.Run(name, func(t *testing.T) {
			direct, err := db.Execute(ctx, q)
			if err != nil {
				t.Fatalf("execute original: %v", err)
			}
			raw, err := protocol.MarshalQuery(WireQuery(q))
			if err != nil {
				t.Fatalf("marshal wire: %v", err)
			}
			wire, err := protocol.UnmarshalQuery(raw)
			if err != nil {
				t.Fatalf("unmarshal wire: %v\n%s", err, raw)
			}
			lowered, err := lowerQuery(wire)
			if err != nil {
				t.Fatalf("lower: %v", err)
			}
			round, err := db.Execute(ctx, lowered)
			if err != nil {
				t.Fatalf("execute round-tripped: %v", err)
			}
			if !reflect.DeepEqual(resultjson.Datum(direct), resultjson.Datum(round)) {
				t.Fatalf("wire round-trip changed the result\n direct: %#v\n round: %#v",
					resultjson.Datum(direct), resultjson.Datum(round))
			}
		})
	}
}

func idOf(tbl string, i int) string { return tbl + string(rune('0'+i)) }

func mustTable(t *testing.T, ctx context.Context, cat *catalog.Catalog, def model.TableDef) {
	t.Helper()
	if _, err := cat.CreateTable(ctx, def); err != nil {
		t.Fatalf("create table %q: %v", def.Name, err)
	}
}

func mustInsert(t *testing.T, ctx context.Context, db *frontend.DB, table string, row lir.Row) {
	t.Helper()
	if err := db.Insert(ctx, table, row); err != nil {
		t.Fatalf("insert into %q: %v", table, err)
	}
}
