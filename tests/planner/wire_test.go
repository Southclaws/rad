package planner

// Wire-contract probes: the strict LIR schema and preflight claims, tested
// at the raw HTTP layer where necessary — the Go client's Marshal path
// normalizes node structs into closed unions, so malformed payloads can
// only be sent by hand.

import (
	"bytes"
	"io"
	"net/http"
	"strconv"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/tests/harness"
)

// postQuery sends a raw LIR body as a one-statement execution program to
// POST /execute and returns status + body. The body is embedded verbatim as
// the statement's relation, so the server's two-phase validation reports the
// same LIR-level rejection it always did — now surfaced through /execute.
func postQuery(t *testing.T, d *harness.DB, body string) (int, string) {
	t.Helper()
	base := strings.Replace(d.URL, "rad://", "http://", 1)
	prog := `{"statements":[{"name":"q","kind":"query","relation":` + body + `}]}`
	res, err := http.Post(base+"/execute", "application/json", bytes.NewReader([]byte(prog)))
	if err != nil {
		t.Fatalf("post: %v", err)
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(res.Body)
	return res.StatusCode, string(b)
}

func TestWireUnknownNodeKindRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	status, body := postQuery(t, d, `{
		"nodes": {"m": {"kind": "mapreduce", "table": "orders"}},
		"root": {"node": "m", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("unknown kind: status %d, want schema rejection (400/422)\n%s", status, body)
	}
	if !strings.Contains(body, `node \"m\": unknown relation kind \"mapreduce\"`) {
		t.Fatalf("rejection does not name the node and kind:\n%s", body)
	}
}

func TestWireCrossVariantFieldRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A scan carrying a filter's payload — closed unions must reject it, and
	// the error must name the node and its variant.
	status, body := postQuery(t, d, `{
		"nodes": {"s": {"kind": "scan", "table": "orders", "scope": "o",
			"predicate": {"kind": "lit", "value": true}}},
		"root": {"node": "s", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("cross-variant field: status %d, want schema rejection\n%s", status, body)
	}
	if !strings.Contains(body, `node \"s\" (scan)`) {
		t.Fatalf("rejection does not name the node and variant:\n%s", body)
	}
}

func TestWireEmptyNodesRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	status, body := postQuery(t, d, `{"nodes": {}, "root": {"node": "x", "cardinality": "many"}}`)
	if status != 400 && status != 422 {
		t.Fatalf("empty nodes: status %d, want rejection\n%s", status, body)
	}
}

func TestWireNegativeLimitRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	status, body := postQuery(t, d, `{
		"nodes": {
			"o": {"kind": "scan", "table": "orders", "scope": "o"},
			"s": {"kind": "slice", "input": "o", "limit": -1}
		},
		"root": {"node": "s", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("negative limit: status %d, want rejection\n%s", status, body)
	}
	// The kind-directed best-match pass names the node, its variant, and the
	// violated rule — no more anonymous oneOf dumps.
	if !strings.Contains(body, `node \"s\" (slice)`) || !strings.Contains(body, "limit") {
		t.Fatalf("rejection does not name node and rule:\n%s", body)
	}
}

func TestWireBadExpressionInsideNode(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A binary comparison missing its right operand: the failure names the
	// node AND drills into the offending expression variant.
	status, body := postQuery(t, d, `{
		"nodes": {
			"o": {"kind": "scan", "table": "orders", "scope": "o"},
			"f": {"kind": "filter", "input": "o",
				"predicate": {"kind": "binary", "op": "eq",
					"left": {"kind": "col", "scope": "o", "column": "status"}}}
		},
		"root": {"node": "f", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("bad expr: status %d, want rejection\n%s", status, body)
	}
	if !strings.Contains(body, `node \"f\" (filter)`) || !strings.Contains(body, "binary") {
		t.Fatalf("rejection does not locate the bad expression:\n%s", body)
	}
}

func TestWireMissingKindRejected(t *testing.T) {
	t.Parallel()
	d := shop(t)
	status, body := postQuery(t, d, `{
		"nodes": {"x": {"table": "orders", "scope": "o"}},
		"root": {"node": "x", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("missing kind: status %d, want rejection\n%s", status, body)
	}
	if !strings.Contains(body, `node \"x\": missing \"kind\"`) {
		t.Fatalf("rejection does not say the kind is missing:\n%s", body)
	}
}

func TestWireRootMustReferenceANode(t *testing.T) {
	t.Parallel()
	d := shop(t)
	status, body := postQuery(t, d, `{
		"nodes": {"o": {"kind": "scan", "table": "orders", "scope": "o"}},
		"root": {"node": "ghost", "cardinality": "many"}
	}`)
	if status != 400 && status != 422 {
		t.Fatalf("dangling root: status %d, want rejection\n%s", status, body)
	}
}

// The response envelope carries one datum shaped exactly as the root
// materialises — a scalar root is a naked value, not a smuggled record.
func TestWireScalarRootIsNakedValue(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"fold": {Kind: "aggregate", Input: "o",
			Aggs: []protocol.AggTerm{{Fn: "count", As: "n"}}},
	}, "fold", "scalar")).EqualsDatum(`7`)
}

func TestWireScalarRootNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// max over the empty set is NULL — the scalar root carries it as null.
	d.Query(q(map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"none": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "status"), protocol.Lit("ghost"))},
		"fold": {Kind: "aggregate", Input: "none",
			Aggs: []protocol.AggTerm{{Fn: "max", Arg: protocol.Col("o", "placed_at"), As: "m"}}},
	}, "fold", "scalar")).EqualsDatum(`null`)
}

func TestWireFirstRootIsObjectOrNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	newest := map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Lit("c4"))},
		"latest": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
		"out": {Kind: "project", Input: "latest",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}
	d.Query(q(newest, "out", "first")).EqualsDatum(`{"id":"o6"}`)

	none := map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
		"mine": {Kind: "filter", Input: "o",
			Predicate: protocol.Eq(protocol.Col("o", "customer_id"), protocol.Lit("c5"))},
		"latest": {Kind: "order", Input: "mine",
			Terms: []protocol.OrderTerm{{Expr: *protocol.Col("o", "placed_at"), Desc: true}}},
	}
	d.Query(q(none, "latest", "first")).EqualsDatum(`null`)
}

func TestWireInt64PrecisionRoundTrip(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// 2^53+1 is unrepresentable in float64 — it must survive the whole
	// pipeline: client create, storage, filter equality, and the response.
	const big = int64(9007199254740993)
	d.Insert("customers", harness.Row{
		"id": "c9", "name": "Max", "email": "max@shop.io", "tier": "gold", "created_at": big,
	})
	d.Query(q(map[string]protocol.Node{
		"c": {Kind: "scan", Table: "customers", Scope: "c"},
		"exact": {Kind: "filter", Input: "c",
			Predicate: protocol.Eq(protocol.Col("c", "created_at"), protocol.Lit(big))},
		"out": {Kind: "project", Input: "exact", Fields: []protocol.Field{
			{As: "id", Expr: *protocol.Col("c", "id")},
			{As: "at", Expr: *protocol.Col("c", "created_at")},
		}},
	}, "out", "many")).Equals(`[{"id":"c9","at":9007199254740993}]`)
}

func TestWireLongConjunction(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A 60-term and-chain: the shape a dynamic filter UI produces.
	preds := make([]*protocol.Expr, 0, 60)
	for range 30 {
		preds = append(preds,
			protocol.Gte(protocol.Col("o", "placed_at"), protocol.Lit(1000)),
			protocol.Lte(protocol.Col("o", "placed_at"), protocol.Lit(3500)),
		)
	}
	preds = append(preds, protocol.Eq(protocol.Col("o", "status"), protocol.Lit("shipped")))
	d.Query(q(map[string]protocol.Node{
		"o":    {Kind: "scan", Table: "orders", Scope: "o"},
		"long": {Kind: "filter", Input: "o", Predicate: protocol.AndAll(preds)},
		"out": {Kind: "project", Input: "long",
			Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}},
	}, "out", "many")).Equals(`[{"id":"o2"}]`)
}

func TestWireDeepNodeChain(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A 60-node filter tower — depth alone must not break decode, preflight,
	// binding, or planning.
	nodes := map[string]protocol.Node{
		"o": {Kind: "scan", Table: "orders", Scope: "o"},
	}
	prev := "o"
	for i := range 60 {
		id := "f" + strconv.Itoa(i)
		nodes[id] = protocol.Node{Kind: "filter", Input: prev,
			Predicate: protocol.Gte(protocol.Col("o", "placed_at"), protocol.Lit(0))}
		prev = id
	}
	nodes["out"] = protocol.Node{Kind: "project", Input: prev,
		Fields: []protocol.Field{{As: "id", Expr: *protocol.Col("o", "id")}}}
	d.Query(q(nodes, "out", "many")).Len(7)
}
