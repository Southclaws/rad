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

	"github.com/Southclaws/rad/rad/protocol/lirwire"
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
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"fold": lirwire.Aggregate("o", "", nil,
			[]lirwire.AggTerm{{Fn: "count", As: "n"}}),
	}, "fold", "scalar")).EqualsDatum(`7`)
}

func TestWireScalarRootNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// max over the empty set is NULL — the scalar root carries it as null.
	d.Query(q(map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"none": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("ghost"))),
		"fold": lirwire.Aggregate("none", "", nil,
			[]lirwire.AggTerm{{Fn: "max", Arg: ptrExpr(lirwire.Col("o", "placed_at")), As: "m"}}),
	}, "fold", "scalar")).EqualsDatum(`null`)
}

func TestWireFirstRootIsObjectOrNull(t *testing.T) {
	t.Parallel()
	d := shop(t)
	newest := map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.LitOf("c4"))),
		"latest": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
		"out": lirwire.Project("latest", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}
	d.Query(q(newest, "out", "first")).EqualsDatum(`{"id":"o6"}`)

	none := map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
		"mine": lirwire.Filter("o",
			lirwire.Binary("eq", lirwire.Col("o", "customer_id"), lirwire.LitOf("c5"))),
		"latest": lirwire.Order("mine",
			[]lirwire.OrderTerm{{Expr: lirwire.Col("o", "placed_at"), Desc: ptrBool(true)}}),
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
	d.Query(q(map[string]lirwire.Node{
		"c": lirwire.Scan("customers", "c"),
		"exact": lirwire.Filter("c",
			lirwire.Binary("eq", lirwire.Col("c", "created_at"), lirwire.LitOf(big))),
		"out": lirwire.Project("exact", "", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("c", "id")},
			{As: "at", Expr: lirwire.Col("c", "created_at")},
		}),
	}, "out", "many")).Equals(`[{"id":"c9","at":9007199254740993}]`)
}

func TestWireLongConjunction(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A 60-term and-chain: the shape a dynamic filter UI produces.
	preds := make([]lirwire.Expr, 0, 60)
	for range 30 {
		preds = append(preds,
			lirwire.Binary("gte", lirwire.Col("o", "placed_at"), lirwire.LitOf(1000)),
			lirwire.Binary("lte", lirwire.Col("o", "placed_at"), lirwire.LitOf(3500)),
		)
	}
	preds = append(preds, lirwire.Binary("eq", lirwire.Col("o", "status"), lirwire.LitOf("shipped")))
	d.Query(q(map[string]lirwire.Node{
		"o":    lirwire.Scan("orders", "o"),
		"long": lirwire.Filter("o", lirwire.AndAll(preds)),
		"out": lirwire.Project("long", "", nil,
			[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}}),
	}, "out", "many")).Equals(`[{"id":"o2"}]`)
}

func TestWireDeepNodeChain(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// A 60-node filter tower — depth alone must not break decode, preflight,
	// binding, or planning.
	nodes := map[string]lirwire.Node{
		"o": lirwire.Scan("orders", "o"),
	}
	prev := "o"
	for i := range 60 {
		id := "f" + strconv.Itoa(i)
		nodes[id] = lirwire.Filter(prev,
			lirwire.Binary("gte", lirwire.Col("o", "placed_at"), lirwire.LitOf(0)))
		prev = id
	}
	nodes["out"] = lirwire.Project(prev, "", nil,
		[]lirwire.Field{{As: "id", Expr: lirwire.Col("o", "id")}})
	d.Query(q(nodes, "out", "many")).Len(7)
}
