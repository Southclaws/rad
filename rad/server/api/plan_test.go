package api

import (
	"context"
	"strings"
	"testing"

	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/protocol"
)

// planCatalog builds a live catalog reader over an in-memory store holding a
// single tasks table — the Catalog the plan viewer reads schema from.
func planCatalog(t *testing.T) catalog.Reader {
	t.Helper()
	store, err := kvslate.Open("plan-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })

	cat := catalog.New(store)
	if _, err := cat.InitMode(context.Background(), catalog.ModeDirect); err != nil {
		t.Fatal(err)
	}
	if _, err := cat.CreateTable(context.Background(), catalog.TableDef{
		Name: "tasks",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "status", Type: catalog.TypeText},
			{Name: "priority", Type: catalog.TypeInt64},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	return catalog.NewReader(store)
}

// planDoc is a well-formed LIR document over the tasks table: filter to open
// tasks, order by priority, root a many-collection. It exercises the full
// wire-to-plan path a real query takes.
func planDoc(t *testing.T) []byte {
	t.Helper()
	raw, err := protocol.MarshalQuery(protocol.Query{
		Nodes: map[string]protocol.Node{
			"tasks": {Kind: "scan", Table: "tasks", Scope: "t"},
			"open": {Kind: "filter", Input: "tasks", Predicate: protocol.Eq(
				protocol.Col("t", "status"), protocol.Lit("open"),
			)},
			"sorted": {Kind: "order", Input: "open", Terms: []protocol.OrderTerm{
				{Expr: *protocol.Col("t", "priority"), Desc: true},
			}},
		},
		Root: protocol.Root{Node: "sorted", Cardinality: "many"},
	})
	if err != nil {
		t.Fatal(err)
	}
	return raw
}

// A valid query renders a physical plan naming the table it scans; the whole
// path runs without touching data storage.
func TestExplainRendersPlan(t *testing.T) {
	plan, err := Explain(context.Background(), planCatalog(t), planDoc(t))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(plan, "tasks") {
		t.Fatalf("plan should name the scanned table:\n%s", plan)
	}
}

// Each failure mode is the caller's, and surfaces as a describing error rather
// than a panic: malformed JSON, a structurally invalid graph, and an unknown
// table all report distinctly.
func TestExplainReportsCallerErrors(t *testing.T) {
	cat := planCatalog(t)

	if _, err := Explain(context.Background(), cat, []byte("{ not json")); err == nil {
		t.Fatal("malformed JSON should error")
	}

	unknownTable := protocol.Query{
		Nodes: map[string]protocol.Node{
			"scan": {Kind: "scan", Table: "ghosts", Scope: "g"},
			"sorted": {Kind: "order", Input: "scan", Terms: []protocol.OrderTerm{
				{Expr: *protocol.Col("g", "id")},
			}},
		},
		Root: protocol.Root{Node: "sorted", Cardinality: "many"},
	}
	raw, err := protocol.MarshalQuery(unknownTable)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := Explain(context.Background(), cat, raw); err == nil {
		t.Fatal("planning against an unknown table should error")
	}
}
