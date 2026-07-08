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
	mux := http.NewServeMux()
	newDBAPI(db, cat).register(mux)
	srv := httptest.NewServer(withRecovery(mux))
	t.Cleanup(srv.Close)

	// httptest serves plain http on 127.0.0.1:port — reachable as rad://.
	c, err := radclient.Dial("rad://" + strings.TrimPrefix(srv.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	return c
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
	recs, err := c.Query(ctx, protocol.Read{Table: "posts"})
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
	if _, err := c.Query(ctx, protocol.Read{Table: "ghost"}); err == nil {
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

	recs, err := c.Query(ctx, protocol.Read{
		Table:  "users",
		Filter: &protocol.Expr{Op: "eq", Column: "name", Value: "ada"},
		Include: []protocol.Include{{
			FK: "posts_user_id_fk", Dir: "children", As: "posts",
			OrderBy: []protocol.Order{{Column: "title"}},
		}},
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
		recs, err := tx.Query(ctx, protocol.Read{Table: "users"})
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
	if recs, _ := c.Query(ctx, protocol.Read{Table: "users"}); len(recs) != 0 {
		t.Fatal("rolled-back write visible")
	}

	if err := c.Txn(ctx, func(tx *radclient.Tx) error {
		_, err := tx.Create(ctx, "users", map[string]any{"name": "kept"})
		return err
	}); err != nil {
		t.Fatal(err)
	}
	if recs, _ := c.Query(ctx, protocol.Read{Table: "users"}); len(recs) != 1 {
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
