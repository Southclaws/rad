package main

// Client ↔ server integration: the radclient runtime speaking the full wire
// protocol against real handlers over real HTTP (httptest), on an in-memory
// SlateDB. This is the wire-level contract test.

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	radclient "rad/client"
	"rad/protocol"
	"rad/rad/01_kv/kvslate"
	catalog "rad/rad/02_catalog"
	frontend "rad/rad/06_frontend"
)

const testSchema = `
tables:
  - name: users
    columns:
      - { name: id,   type: string, pk: true, default: uuid() }
      - { name: name, type: string, unique: true }
      - { name: age,  type: int64, nullable: true }
  - name: posts
    columns:
      - { name: id,      type: string, pk: true, default: uuid() }
      - { name: user_id, type: string, ref: users.id, index: true }
      - { name: title,   type: string }
      - { name: score,   type: int64, default: 0 }
`

func testServer(t *testing.T) *radclient.Client {
	t.Helper()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })

	cat := catalog.New(store)
	db := frontend.Open(store)
	api, err := newDBAPI(db, cat).httpHandler(http.NotFoundHandler())
	if err != nil {
		t.Fatal(err)
	}
	srv := httptest.NewServer(withRecovery(api))
	t.Cleanup(srv.Close)

	// httptest serves plain http on 127.0.0.1:port — reachable as rad://.
	c, err := radclient.Dial("rad://" + strings.TrimPrefix(srv.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	return c
}

// tableQ is the simplest graph: scan every row of one table.
func tableQ(table string) protocol.Query {
	return protocol.Query{
		Nodes: map[string]protocol.Node{"t": {Kind: "scan", Table: table, Scope: "t"}},
		Root:  protocol.Root{Node: "t", Cardinality: "many"},
	}
}

// filteredQ scans a table and filters it.
func filteredQ(table string, pred *protocol.Expr) protocol.Query {
	return protocol.Query{
		Nodes: map[string]protocol.Node{
			"t": {Kind: "scan", Table: table, Scope: "t"},
			"f": {Kind: "filter", Input: "t", Predicate: pred},
		},
		Root: protocol.Root{Node: "f", Cardinality: "many"},
	}
}

func migrated(t *testing.T) *radclient.Client {
	t.Helper()
	c := testServer(t)
	if _, err := c.Migrate(context.Background(), testSchema); err != nil {
		t.Fatal(err)
	}
	return c
}

func TestClientPingAndMigrate(t *testing.T) {
	c := testServer(t)
	ctx := context.Background()

	if err := c.Ping(ctx); err != nil {
		t.Fatal(err)
	}
	steps, err := c.Migrate(ctx, testSchema)
	if err != nil {
		t.Fatal(err)
	}
	if len(steps) != 2 {
		t.Fatalf("steps = %v", steps)
	}
	// Idempotent.
	steps, err = c.Migrate(ctx, testSchema)
	if err != nil || len(steps) != 0 {
		t.Fatalf("re-migrate: steps=%v err=%v", steps, err)
	}
	tables, err := c.Tables(ctx)
	if err != nil || len(tables) != 2 {
		t.Fatalf("tables=%v err=%v", tables, err)
	}
}

func TestClientCRUDOverTheWire(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	// Create returns server-applied defaults.
	user, err := c.Create(ctx, "users", map[string]any{"name": "ada"})
	if err != nil {
		t.Fatal(err)
	}
	id, _ := user["id"].(string)
	if len(id) != 36 {
		t.Fatalf("no uuid default: %v", user)
	}

	// Get round-trips; int64 precision survives json.Number.
	big := int64(9007199254740993) // > 2^53, breaks float64 JSON decoding
	if _, err := c.Create(ctx, "posts", map[string]any{"user_id": id, "title": "t", "score": big}); err != nil {
		t.Fatal(err)
	}
	recs, err := c.Query(ctx, tableQ("posts"))
	if err != nil || len(recs) != 1 {
		t.Fatalf("recs=%v err=%v", recs, err)
	}
	if n, _ := recs[0]["score"].(interface{ Int64() (int64, error) }); n != nil {
		got, _ := n.Int64()
		if got != big {
			t.Fatalf("int64 precision lost: %v", recs[0]["score"])
		}
	} else {
		t.Fatalf("score not a json.Number: %T", recs[0]["score"])
	}

	// Update + clear-to-NULL.
	if _, _, err := c.Update(ctx, "users", map[string]any{"id": id}, map[string]any{"age": 40}, nil); err != nil {
		t.Fatal(err)
	}
	got, found, err := c.Get(ctx, "users", map[string]any{"id": id})
	if err != nil || !found || got["age"] == nil {
		t.Fatalf("age update lost: %v", got)
	}
	if _, _, err := c.Update(ctx, "users", map[string]any{"id": id}, nil, []string{"age"}); err != nil {
		t.Fatal(err)
	}
	got, _, _ = c.Get(ctx, "users", map[string]any{"id": id})
	if got["age"] != nil {
		t.Fatalf("clear-to-NULL failed: %v", got)
	}

	// Delete respects restrict, then succeeds bottom-up.
	if _, err := c.Delete(ctx, "users", map[string]any{"id": id}); err == nil {
		t.Fatal("restricted delete succeeded")
	}
}

// Value-less expression ops (and/or/not/is_null) must encode cleanly: the
// generated wire codec emits the `value` field unconditionally, so the
// converter has to supply an explicit JSON null rather than leave the raw
// value empty (which produced the malformed body `"value":}`).
func TestClientExprOpsOverTheWire(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	if _, err := c.Create(ctx, "users", map[string]any{"name": "ada", "age": 40}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Create(ctx, "users", map[string]any{"name": "bob"}); err != nil {
		t.Fatal(err)
	}

	// A binary `and` — an expr with no value of its own.
	recs, err := c.Query(ctx, filteredQ("users", protocol.AndAll([]*protocol.Expr{
		protocol.Eq(protocol.Col("t", "name"), protocol.Lit("ada")),
		protocol.Gte(protocol.Col("t", "age"), protocol.Lit(18)),
	})))
	if err != nil {
		t.Fatalf("and filter: %v", err)
	}
	if len(recs) != 1 || recs[0]["name"] != "ada" {
		t.Fatalf("and filter recs = %v", recs)
	}

	// is_null carries only its operand.
	recs, err = c.Query(ctx, filteredQ("users",
		protocol.IsNull(protocol.Col("t", "age"))))
	if err != nil {
		t.Fatalf("is_null filter: %v", err)
	}
	if len(recs) != 1 || recs[0]["name"] != "bob" {
		t.Fatalf("is_null recs = %v", recs)
	}

	// not wraps a sub-expression and has no value either.
	recs, err = c.Query(ctx, filteredQ("users",
		protocol.Not(protocol.IsNull(protocol.Col("t", "age")))))
	if err != nil {
		t.Fatalf("not filter: %v", err)
	}
	if len(recs) != 1 || recs[0]["name"] != "ada" {
		t.Fatalf("not recs = %v", recs)
	}
}

// Errors arrive as RFC 7807 problems with stable codes.
func TestClientProblemDetails(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	if _, err := c.Create(ctx, "users", map[string]any{"name": "dup"}); err != nil {
		t.Fatal(err)
	}
	_, err := c.Create(ctx, "users", map[string]any{"name": "dup"})
	var ae *radclient.APIError
	if !asAPIError(err, &ae) {
		t.Fatalf("want APIError, got %v", err)
	}
	pb := ae.Problem
	if pb.Code != protocol.CodeInvalid || pb.Status != 422 ||
		pb.Type != protocol.ProblemTypeBase+protocol.CodeInvalid || pb.Title == "" {
		t.Fatalf("problem = %+v", pb)
	}
	if !strings.Contains(pb.Detail, "unique index") {
		t.Fatalf("detail = %q", pb.Detail)
	}

	// Unknown table → invalid; unknown tx → not_found.
	if _, err := c.Query(ctx, tableQ("ghost")); err == nil {
		t.Fatal("unknown table accepted")
	}
}

func TestClientNestedQuery(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	user, _ := c.Create(ctx, "users", map[string]any{"name": "ada"})
	id := user["id"].(string)
	for _, title := range []string{"b", "a"} {
		if _, err := c.Create(ctx, "posts", map[string]any{"user_id": id, "title": title}); err != nil {
			t.Fatal(err)
		}
	}

	recs, err := c.Query(ctx, protocol.Query{
		Nodes: map[string]protocol.Node{
			"users": {Kind: "scan", Table: "users", Scope: "u"},
			"ada": {Kind: "filter", Input: "users",
				Predicate: protocol.Eq(protocol.Col("u", "name"), protocol.Lit("ada"))},
			"posts": {Kind: "scan", Table: "posts", Scope: "p"},
			"theirs": {Kind: "filter", Input: "posts",
				Predicate: protocol.Eq(protocol.Col("p", "user_id"), protocol.Col("u", "id"))},
			"sorted": {Kind: "order", Input: "theirs",
				Terms: []protocol.OrderTerm{{Expr: *protocol.Col("p", "title")}}},
			"out": {Kind: "project", Input: "ada", Spread: []string{"u"},
				Fields: []protocol.Field{{As: "posts", Expr: *protocol.ArrayOf("sorted")}}},
		},
		Root: protocol.Root{Node: "out", Cardinality: "many"},
	})
	if err != nil || len(recs) != 1 {
		t.Fatalf("recs=%d err=%v", len(recs), err)
	}
	posts, _ := recs[0]["posts"].([]any)
	if len(posts) != 2 {
		t.Fatalf("posts = %v", recs[0]["posts"])
	}
	first, _ := posts[0].(map[string]any)
	if first["title"] != "a" {
		t.Fatalf("child ordering lost: %v", posts)
	}
}

// Aggregation rides the query endpoint: a Read with Aggs returns one record
// of scalars, and an aggregate include folds children in the same round trip.
func TestClientAggregateOverTheWire(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	user, _ := c.Create(ctx, "users", map[string]any{"name": "ada"})
	id := user["id"].(string)
	for _, score := range []int64{10, 20, 30} {
		if _, err := c.Create(ctx, "posts", map[string]any{
			"user_id": id, "title": "t", "score": score,
		}); err != nil {
			t.Fatal(err)
		}
	}

	// A global fold: one record of scalars, no rows shipped.
	recs, err := c.Query(ctx, protocol.Query{
		Nodes: map[string]protocol.Node{
			"posts": {Kind: "scan", Table: "posts", Scope: "p"},
			"stats": {Kind: "aggregate", Input: "posts", Aggs: []protocol.AggTerm{
				{Fn: "count", As: "n"},
				{Fn: "sum", Arg: protocol.Col("p", "score"), As: "total"},
				{Fn: "avg", Arg: protocol.Col("p", "score"), As: "mean"},
				{Fn: "max", Arg: protocol.Col("p", "score"), As: "top"},
			}},
		},
		Root: protocol.Root{Node: "stats", Cardinality: "exactly_one"},
	})
	if err != nil || len(recs) != 1 {
		t.Fatalf("recs=%d err=%v", len(recs), err)
	}
	if got := jsonInt(t, recs[0]["n"]); got != 3 {
		t.Errorf("count = %d, want 3", got)
	}
	if got := jsonInt(t, recs[0]["total"]); got != 60 {
		t.Errorf("sum = %d, want 60", got)
	}
	if got := jsonFloat(t, recs[0]["mean"]); got != 20 {
		t.Errorf("avg = %v, want 20", got)
	}
	if got := jsonInt(t, recs[0]["top"]); got != 30 {
		t.Errorf("max = %d, want 30", got)
	}

	// A folded relation as a field: the board-card shape — a user with a
	// nested object of post statistics.
	recs, err = c.Query(ctx, protocol.Query{
		Nodes: map[string]protocol.Node{
			"users": {Kind: "scan", Table: "users", Scope: "u"},
			"ada": {Kind: "filter", Input: "users",
				Predicate: protocol.Eq(protocol.Col("u", "name"), protocol.Lit("ada"))},
			"posts": {Kind: "scan", Table: "posts", Scope: "p"},
			"theirs": {Kind: "filter", Input: "posts",
				Predicate: protocol.Eq(protocol.Col("p", "user_id"), protocol.Col("u", "id"))},
			"folded": {Kind: "aggregate", Input: "theirs",
				Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
			"out": {Kind: "project", Input: "ada", Spread: []string{"u"},
				Fields: []protocol.Field{{As: "post_stats", Expr: *protocol.FirstOf("folded")}}},
		},
		Root: protocol.Root{Node: "out", Cardinality: "many"},
	})
	if err != nil || len(recs) != 1 {
		t.Fatalf("recs=%d err=%v", len(recs), err)
	}
	stats, ok := recs[0]["post_stats"].(map[string]any)
	if !ok {
		t.Fatalf("post_stats is not a scalar object: %T %v", recs[0]["post_stats"], recs[0]["post_stats"])
	}
	if got := jsonInt(t, stats["n"]); got != 3 {
		t.Errorf("include count = %d, want 3", got)
	}

	// GROUP BY: a grouped fold returns one record per distinct key, and the
	// ordering above it addresses the aggregate's output scope — relational
	// closure over the wire.
	if _, err := c.Create(ctx, "posts", map[string]any{"user_id": id, "title": "z", "score": int64(30)}); err != nil {
		t.Fatal(err)
	}
	recs, err = c.Query(ctx, protocol.Query{
		Nodes: map[string]protocol.Node{
			"posts": {Kind: "scan", Table: "posts", Scope: "p"},
			"stats": {Kind: "aggregate", Input: "posts", Scope: "stats",
				Groups: []protocol.GroupTerm{{Expr: *protocol.Col("p", "score")}},
				Aggs:   []protocol.AggTerm{{Fn: "count", As: "n"}}},
			"sorted": {Kind: "order", Input: "stats",
				Terms: []protocol.OrderTerm{{Expr: *protocol.Col("stats", "score"), Desc: true}}},
		},
		Root: protocol.Root{Node: "sorted", Cardinality: "many"},
	})
	if err != nil || len(recs) != 3 {
		t.Fatalf("grouped fold: recs=%d err=%v", len(recs), err)
	}
	if got := jsonInt(t, recs[0]["score"]); got != 30 {
		t.Fatalf("group order lost: %v", recs)
	}
	if got := jsonInt(t, recs[0]["n"]); got != 2 {
		t.Fatalf("group count = %d, want 2 (two score-30 posts)", got)
	}

	// Bind-time rejections surface as 422 invalid problems: a dangling node
	// reference, and a fold over a non-numeric argument.
	_, err = c.Query(ctx, protocol.Query{
		Nodes: map[string]protocol.Node{"posts": {Kind: "scan", Table: "posts", Scope: "p"}},
		Root:  protocol.Root{Node: "ghost", Cardinality: "many"},
	})
	if err == nil || !strings.Contains(err.Error(), `unknown node "ghost"`) {
		t.Fatalf("dangling node reference accepted: %v", err)
	}
	_, err = c.Query(ctx, protocol.Query{
		Nodes: map[string]protocol.Node{
			"posts": {Kind: "scan", Table: "posts", Scope: "p"},
			"bad": {Kind: "aggregate", Input: "posts",
				Aggs: []protocol.AggTerm{{Fn: "sum", Arg: protocol.Col("p", "title"), As: "s"}}},
		},
		Root: protocol.Root{Node: "bad", Cardinality: "exactly_one"},
	})
	if err == nil || !strings.Contains(err.Error(), "numeric") {
		t.Fatalf("sum over text accepted: %v", err)
	}
}

func jsonInt(t *testing.T, v any) int64 {
	t.Helper()
	n, ok := v.(interface{ Int64() (int64, error) })
	if !ok {
		t.Fatalf("not a json.Number: %T (%v)", v, v)
	}
	got, err := n.Int64()
	if err != nil {
		t.Fatal(err)
	}
	return got
}

func jsonFloat(t *testing.T, v any) float64 {
	t.Helper()
	n, ok := v.(interface{ Float64() (float64, error) })
	if !ok {
		t.Fatalf("not a json.Number: %T (%v)", v, v)
	}
	got, err := n.Float64()
	if err != nil {
		t.Fatal(err)
	}
	return got
}

// Transactions over the wire: rollback discards, commit persists, racing
// writes conflict with a retryable code.
func TestClientTransactions(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	err := c.Txn(ctx, func(tx *radclient.Tx) error {
		if _, err := tx.Create(ctx, "users", map[string]any{"name": "ghost"}); err != nil {
			return err
		}
		// Read-your-writes inside the session.
		recs, err := tx.Query(ctx, tableQ("users"))
		if err != nil {
			return err
		}
		if len(recs) != 1 {
			t.Errorf("tx sees %d users", len(recs))
		}
		return errAbort
	})
	if err != errAbort {
		t.Fatalf("fn error not propagated: %v", err)
	}
	if recs, _ := c.Query(ctx, tableQ("users")); len(recs) != 0 {
		t.Fatal("rolled-back write visible")
	}

	if err := c.Txn(ctx, func(tx *radclient.Tx) error {
		_, err := tx.Create(ctx, "users", map[string]any{"name": "kept"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if recs, _ := c.Query(ctx, tableQ("users")); len(recs) != 1 {
		t.Fatal("committed write missing")
	}

	// Conflict: racing duplicate-name inserts (disjoint writes, tracked
	// unique-check ranges collide).
	err = c.Txn(ctx, func(tx *radclient.Tx) error {
		if _, err := tx.Create(ctx, "users", map[string]any{"name": "race"}); err != nil {
			return err
		}
		_, err := c.Create(ctx, "users", map[string]any{"name": "race"})
		return err
	})
	if !radclient.IsConflict(err) {
		t.Fatalf("expected conflict, got %v", err)
	}
}

var errAbort = &abortErr{}

type abortErr struct{}

func (*abortErr) Error() string { return "abort" }

func asAPIError(err error, target **radclient.APIError) bool {
	for err != nil {
		if ae, ok := err.(*radclient.APIError); ok {
			*target = ae
			return true
		}
		u, ok := err.(interface{ Unwrap() error })
		if !ok {
			return false
		}
		err = u.Unwrap()
	}
	return false
}
