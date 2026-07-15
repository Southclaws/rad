// Package planner battle-tests the query engine end to end: raw LIR trees
// through the real client and server, schema by direct catalog mutation, no
// codegen anywhere. Every test writes out the literal wire query it sends —
// the node map IS the test — and asserts the exact rows that come back.
package planner

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/tests/harness"
)

// tracker is the shared fixture: the boards/tasks/users shape that exercises
// point gets, index prefixes, correlation, and folds. Tests that need a
// different schema build their own inline — a fixture is a convenience,
// never a constraint.
func tracker(t *testing.T) *harness.DB {
	d := harness.New(t)

	d.Table("users",
		harness.Text("id"), harness.Text("name"),
	).Unique("users_name_uq", "name").Create()

	d.Table("boards",
		harness.Text("id"), harness.Text("name"), harness.Text("owner_id"),
	).FK("boards_owner_fk", "owner_id", "users", "id").Create()

	d.Table("tasks",
		harness.Text("id"), harness.Text("board_id"), harness.Text("title"),
		harness.Text("status"), harness.Int64("priority"),
		harness.Null(harness.Float64("estimate")), harness.Null(harness.Text("assignee_id")),
	).
		Index("tasks_board_status_idx", "board_id", "status").
		FK("tasks_board_fk", "board_id", "boards", "id").
		FK("tasks_assignee_fk", "assignee_id", "users", "id").
		Create()

	d.Insert("users",
		harness.Row{"id": "ada", "name": "Ada"},
		harness.Row{"id": "bob", "name": "Bob"},
	)
	d.Insert("boards",
		harness.Row{"id": "b1", "name": "Launch", "owner_id": "ada"},
		harness.Row{"id": "b2", "name": "Infra", "owner_id": "bob"},
	)
	d.Insert("tasks",
		harness.Row{"id": "t1", "board_id": "b1", "title": "ship", "status": "open", "priority": 3, "assignee_id": "bob", "estimate": 2.0},
		harness.Row{"id": "t2", "board_id": "b1", "title": "write", "status": "open", "priority": 5},
		harness.Row{"id": "t3", "board_id": "b1", "title": "done", "status": "done", "priority": 9, "assignee_id": "ada"},
		harness.Row{"id": "t4", "board_id": "b2", "title": "rack", "status": "open", "priority": 1, "assignee_id": "ada", "estimate": 8.0},
	)
	return d
}

// -
// access paths
// -

func TestPointGetByPrimaryKey(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"by_id": lirwire.Filter("t",
			lirwire.Binary("eq", lirwire.Col("t", "id"), lirwire.LitOf("t2"))),
	}, "by_id", "many")).Equals(`[
		{"id":"t2","board_id":"b1","title":"write","status":"open","priority":5,"estimate":null,"assignee_id":null}
	]`)
}

func TestIndexPrefixWithResidual(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// board_id+status ride the index; the whole predicate still filters.
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"open_b1": lirwire.Filter("t",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("t", "board_id"), lirwire.LitOf("b1")),
				lirwire.Binary("eq", lirwire.Col("t", "status"), lirwire.LitOf("open")),
				lirwire.Binary("gt", lirwire.Col("t", "priority"), lirwire.LitOf(4)),
			})),
		"ids": lirwire.Project("open_b1", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("t", "id")}}),
	}, "ids", "many")).Equals(`[{"id":"t2"}]`)
}

func TestOrderedIndexWithLimit(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"b1": lirwire.Filter("t",
			lirwire.Binary("eq", lirwire.Col("t", "board_id"), lirwire.LitOf("b1"))),
		"by_priority": lirwire.Order("b1",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("t", "priority"), Desc: ptrBool(true)}}),
		"top2": lirwire.Slice("by_priority", 0, ptrInt(2)),
		"out": lirwire.Project("top2", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("t", "id")},
			{As: "priority", Expr: lirwire.Col("t", "priority")},
		}),
	}, "out", "many")).Equals(`[{"id":"t3","priority":9},{"id":"t2","priority":5}]`)
}

// -
// three-valued logic
// -

func TestNotEqSkipsNulls(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// NOT (assignee = 'ada') must not match tasks with a NULL assignee.
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"not_ada": lirwire.Filter("t",
			lirwire.Unary("not", lirwire.Binary("eq", lirwire.Col("t", "assignee_id"), lirwire.LitOf("ada")))),
		"by_id": lirwire.Order("not_ada",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("t", "id")}}),
		"out": lirwire.Project("by_id", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("t", "id")}}),
	}, "out", "many")).Equals(`[{"id":"t1"}]`)
}

func TestIsNullIsTheOnlyNullMatch(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"unassigned": lirwire.Filter("t",
			lirwire.Unary("is_null", lirwire.Col("t", "assignee_id"))),
		"out": lirwire.Project("unassigned", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("t", "id")}}),
	}, "out", "many")).Equals(`[{"id":"t2"}]`)
}

// -
// crossings
// -

func TestNestedShape(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// Each board with its owner (to-parent first) and its open tasks by
	// priority (correlated array) — the forcing shape, hand-written.
	d.Query(q(map[string]lirwire.Node{
		"b": lirwire.Scan("boards", "b"),
		"boards_by_id": lirwire.Order("b",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("b", "id")}}),
		"o": lirwire.Scan("users", "o"),
		"owner": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "id"), lirwire.Col("b", "owner_id"))),
		"t": lirwire.Scan("tasks", "t"),
		"open": lirwire.Filter("t",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("t", "board_id"), lirwire.Col("b", "id")),
				lirwire.Binary("eq", lirwire.Col("t", "status"), lirwire.LitOf("open")),
			})),
		"open_by_priority": lirwire.Order("open",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("t", "priority"), Desc: ptrBool(true)}}),
		"titles": lirwire.Project("open_by_priority", "", nil,
			[]lirwire.Field{{As: "title", Expr: lirwire.Col("t", "title")}}),
		"out": lirwire.Project("boards_by_id", "", nil, []lirwire.Field{
			{As: "board", Expr: lirwire.Col("b", "name")},
			{As: "owner", Expr: lirwire.First("owner")},
			{As: "open", Expr: lirwire.Array("titles")},
		}),
	}, "out", "many")).Equals(`[
		{"board":"Launch","owner":{"id":"ada","name":"Ada"},"open":[{"title":"write"},{"title":"ship"}]},
		{"board":"Infra","owner":{"id":"bob","name":"Bob"},"open":[{"title":"rack"}]}
	]`)
}

func TestExistsInFilter(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]lirwire.Node{
		"b": lirwire.Scan("boards", "b"),
		"t": lirwire.Scan("tasks", "t"),
		"done_here": lirwire.Filter("t",
			lirwire.AndAll([]lirwire.Expr{
				lirwire.Binary("eq", lirwire.Col("t", "board_id"), lirwire.Col("b", "id")),
				lirwire.Binary("eq", lirwire.Col("t", "status"), lirwire.LitOf("done")),
			})),
		"with_done": lirwire.Filter("b", lirwire.Exists("done_here")),
		"out": lirwire.Project("with_done", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("b", "id")}}),
	}, "out", "many")).Equals(`[{"id":"b1"}]`)
}

// -
// aggregation
// -

func TestGroupedFold(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"stats": lirwire.Aggregate("t", "stats",
			[]lirwire.GroupTerm{{Expr: lirwire.Col("t", "status")}},
			[]lirwire.AggTerm{
				{Fn: "count", As: "n"},
				{Fn: "avg", Arg: ptrExpr(lirwire.Col("t", "priority")), As: "avg_priority"},
			}),
		"by_status": lirwire.Order("stats",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("stats", "status")}}),
	}, "by_status", "many")).Equals(`[
		{"status":"done","n":1,"avg_priority":9},
		{"status":"open","n":3,"avg_priority":3}
	]`)
}

func TestGlobalFoldOverNothing(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// count of an empty set is 0; every other fold is NULL.
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"none": lirwire.Filter("t",
			lirwire.Binary("eq", lirwire.Col("t", "status"), lirwire.LitOf("ghost"))),
		"fold": lirwire.Aggregate("none", "", nil, []lirwire.AggTerm{
			{Fn: "count", As: "n"},
			{Fn: "max", Arg: ptrExpr(lirwire.Col("t", "priority")), As: "worst"},
		}),
	}, "fold", "exactly_one")).Equals(`[{"n":0,"worst":null}]`)
}

// -
// joins
// -

func TestInnerJoinProjection(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"u": lirwire.Scan("users", "u"),
		"assigned": lirwire.Join("t", "u", "inner",
			lirwire.Binary("eq", lirwire.Col("t", "assignee_id"), lirwire.Col("u", "id"))),
		"out": lirwire.Project("assigned", "", nil, []lirwire.Field{
			{As: "task", Expr: lirwire.Col("t", "title")},
			{As: "who", Expr: lirwire.Col("u", "name")},
		}),
	}, "out", "many")).EqualsUnordered(`[
		{"task":"ship","who":"Bob"},
		{"task":"done","who":"Ada"},
		{"task":"rack","who":"Ada"}
	]`)
}

// -
// the error contract
// -

func TestBinderRejections(t *testing.T) {
	t.Parallel()
	d := tracker(t)

	d.Query(q(map[string]lirwire.Node{
		"g": lirwire.Scan("ghosts", "g"),
	}, "g", "many")).ExpectStatus(422).ExpectError(`unknown table "ghosts"`)

	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
		"bad": lirwire.Filter("t",
			lirwire.Binary("eq", lirwire.Col("t", "nope"), lirwire.LitOf(1))),
	}, "bad", "many")).ExpectError(`no column "nope"`)

	// first over an unordered many-row relation is nondeterministic — rejected.
	d.Query(q(map[string]lirwire.Node{
		"t": lirwire.Scan("tasks", "t"),
	}, "t", "first")).ExpectError("add an order or make the relation at-most-one")
}
