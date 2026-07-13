// Package planner battle-tests the query engine end to end: raw LIR trees
// through the real client and server, schema by direct catalog mutation, no
// codegen anywhere. Every test writes out the literal wire query it sends —
// the node map IS the test — and asserts the exact rows that come back.
package planner

import (
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/tests/harness"
)

// q is the query shape under test: named relation nodes plus a root selector.
func q(nodes map[string]protocol.Node, root string, cardinality string) protocol.Query {
	return protocol.Query{Nodes: nodes, Root: protocol.Root{Node: root, Cardinality: cardinality}}
}

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

// ── access paths ────────────────────────────────────────────────────────────

func TestPointGetByPrimaryKey(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"by_id": {Kind: "filter", Input: "t",
			Predicate: protocol.Eq(protocol.Col("t", "id"), protocol.Lit("t2"))},
	}, "by_id", "many")).Equals(`[
		{"id":"t2","board_id":"b1","title":"write","status":"open","priority":5,"estimate":null,"assignee_id":null}
	]`)
}

func TestIndexPrefixWithResidual(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// board_id+status ride the index; the whole predicate still filters.
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"open_b1": {Kind: "filter", Input: "t",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("t", "board_id"), protocol.Lit("b1")),
				protocol.Eq(protocol.Col("t", "status"), protocol.Lit("open")),
				protocol.Gt(protocol.Col("t", "priority"), protocol.Lit(4)),
			})},
		"ids": {Kind: "project", Input: "open_b1",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("t", "id")}}},
	}, "ids", "many")).Equals(`[{"id":"t2"}]`)
}

func TestOrderedIndexWithLimit(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"b1": {Kind: "filter", Input: "t",
			Predicate: protocol.Eq(protocol.Col("t", "board_id"), protocol.Lit("b1"))},
		"by_priority": {Kind: "order", Input: "b1",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("t", "priority"), Desc: true}}},
		"top2": {Kind: "slice", Input: "by_priority", Limit: new(int(2))},
		"out": {Kind: "project", Input: "top2", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("t", "id")},
			{As: "priority", Expr: *protocol.Col("t", "priority")},
		}},
	}, "out", "many")).Equals(`[{"id":"t3","priority":9},{"id":"t2","priority":5}]`)
}

// ── three-valued logic ──────────────────────────────────────────────────────

func TestNotEqSkipsNulls(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// NOT (assignee = 'ada') must not match tasks with a NULL assignee.
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"not_ada": {Kind: "filter", Input: "t",
			Predicate: protocol.Not(protocol.Eq(protocol.Col("t", "assignee_id"), protocol.Lit("ada")))},
		"by_id": {Kind: "order", Input: "not_ada",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("t", "id")}}},
		"out": {Kind: "project", Input: "by_id",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("t", "id")}}},
	}, "out", "many")).Equals(`[{"id":"t1"}]`)
}

func TestIsNullIsTheOnlyNullMatch(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"unassigned": {Kind: "filter", Input: "t",
			Predicate: protocol.IsNull(protocol.Col("t", "assignee_id"))},
		"out": {Kind: "project", Input: "unassigned",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("t", "id")}}},
	}, "out", "many")).Equals(`[{"id":"t2"}]`)
}

// ── crossings ───────────────────────────────────────────────────────────────

func TestNestedShape(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// Each board with its owner (to-parent first) and its open tasks by
	// priority (correlated array) — the forcing shape, hand-written.
	d.Query(q(map[string]protocol.Node{
		"b": {Kind: "scan", Table: "boards", Scope: "b"},
		"o": {Kind: "scan", Table: "users", Scope: "o"},
		"owner": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "id"), protocol.Col("b", "owner_id"))},
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"open": {Kind: "filter", Input: "t",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("t", "board_id"), protocol.Col("b", "id")),
				protocol.Eq(protocol.Col("t", "status"), protocol.Lit("open")),
			})},
		"open_by_priority": {Kind: "order", Input: "open",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("t", "priority"), Desc: true}}},
		"titles": {Kind: "project", Input: "open_by_priority",
			Fields: []protocol.Field{{As: "title", Expr: *protocol.Col("t", "title")}}},
		"out": {Kind: "project", Input: "b", Fields: []protocol.Field{
			{As: "board", Expr: *protocol.Col("b", "name")},
			{As: "owner", Expr: *protocol.FirstOf("owner")},
			{As: "open", Expr: *protocol.ArrayOf("titles")},
		}},
	}, "out", "many")).Equals(`[
		{"board":"Launch","owner":{"id":"ada","name":"Ada"},"open":[{"title":"write"},{"title":"ship"}]},
		{"board":"Infra","owner":{"id":"bob","name":"Bob"},"open":[{"title":"rack"}]}
	]`)
}

func TestExistsInFilter(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]protocol.Node{
		"b": {Kind: "scan", Table: "boards", Scope: "b"},
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"done_here": {Kind: "filter", Input: "t",
			Predicate: protocol.AndAll([]*protocol.Expr{
				protocol.Eq(protocol.Col("t", "board_id"), protocol.Col("b", "id")),
				protocol.Eq(protocol.Col("t", "status"), protocol.Lit("done")),
			})},
		"with_done": {Kind: "filter", Input: "b", Predicate: protocol.Exists("done_here")},
		"out": {Kind: "project", Input: "with_done",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("b", "id")}}},
	}, "out", "many")).Equals(`[{"id":"b1"}]`)
}

// ── aggregation ─────────────────────────────────────────────────────────────

func TestGroupedFold(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"stats": {Kind: "aggregate", Input: "t", Scope: "stats",
			Groups: []protocol.GroupTerm{{Expr: *protocol.Col("t", "status")}},
			Aggs: []protocol.AggTerm{
				{Fn: "count", As: "n"},
				{Fn: "avg", Arg: protocol.Col("t", "priority"), As: "avg_priority"},
			}},
		"by_status": {Kind: "order", Input: "stats",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("stats", "status")}}},
	}, "by_status", "many")).Equals(`[
		{"status":"done","n":1,"avg_priority":9},
		{"status":"open","n":3,"avg_priority":3}
	]`)
}

func TestGlobalFoldOverNothing(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	// count of an empty set is 0; every other fold is NULL.
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"none": {Kind: "filter", Input: "t",
			Predicate: protocol.Eq(protocol.Col("t", "status"), protocol.Lit("ghost"))},
		"fold": {Kind: "aggregate", Input: "none", Aggs: []protocol.AggTerm{
			{Fn: "count", As: "n"},
			{Fn: "max", Arg: protocol.Col("t", "priority"), As: "worst"},
		}},
	}, "fold", "exactly_one")).Equals(`[{"n":0,"worst":null}]`)
}

// ── joins ───────────────────────────────────────────────────────────────────

func TestInnerJoinProjection(t *testing.T) {
	t.Parallel()
	d := tracker(t)
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"u": {Kind: "scan", Table: "users", Scope: "u"},
		"assigned": {Kind: "join", Left: "t", Right: "u", Join: "inner",
			On: protocol.Eq(protocol.Col("t", "assignee_id"), protocol.Col("u", "id"))},
		"out": {Kind: "project", Input: "assigned", Fields: []protocol.Field{
			{As: "task", Expr: *protocol.Col("t", "title")},
			{As: "who", Expr: *protocol.Col("u", "name")},
		}},
	}, "out", "many")).EqualsUnordered(`[
		{"task":"ship","who":"Bob"},
		{"task":"done","who":"Ada"},
		{"task":"rack","who":"Ada"}
	]`)
}

// ── the error contract ──────────────────────────────────────────────────────

func TestBinderRejections(t *testing.T) {
	t.Parallel()
	d := tracker(t)

	d.Query(q(map[string]protocol.Node{
		"g": {Kind: "scan", Table: "ghosts", Scope: "g"},
	}, "g", "many")).ExpectStatus(422).ExpectError(`unknown table "ghosts"`)

	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
		"bad": {Kind: "filter", Input: "t",
			Predicate: protocol.Eq(protocol.Col("t", "nope"), protocol.Lit(1))},
	}, "bad", "many")).ExpectError(`no column "nope"`)

	// first over an unordered many-row relation is nondeterministic — rejected.
	d.Query(q(map[string]protocol.Node{
		"t": {Kind: "scan", Table: "tasks", Scope: "t"},
	}, "t", "first")).ExpectError("add an order or make the relation at-most-one")
}
