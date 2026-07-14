package api

// Wire-level contract tests for /execute: query programs over real handlers
// on an in-memory SlateDB, including statement-result composition (a later
// statement referencing an earlier one by name through a ref), the result
// selector, and the program-envelope rejections.

import (
	"context"
	"errors"
	"strings"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol"
)

// oneRow builds a one-row int64 constant relation — a self-contained query
// body needing no fixture.
func oneRow(col string, v int) protocol.Query {
	return protocol.Query{
		Nodes: map[string]protocol.Node{
			"r": {Kind: "rows", Scope: "r",
				Columns: []protocol.RowsColumn{{Name: col, Type: "int64"}},
				Rows:    [][]any{{v}}},
			"o": {Kind: "order", Input: "r",
				Terms: []protocol.OrderTerm{{Expr: *protocol.Col("r", col)}}},
		},
		Root: protocol.Root{Node: "o", Cardinality: "many"},
	}
}

// A single-statement query program returns that statement's datum with no
// result selector.
func TestExecuteSingleQuery(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	res, err := c.Execute(ctx, protocol.QueryProgram(oneRow("n", 7)))
	if err != nil {
		t.Fatal(err)
	}
	rows, ok := res.Result.([]any)
	if !ok || len(rows) != 1 {
		t.Fatalf("result = %#v, want one row", res.Result)
	}
	if len(res.Statements) != 1 || res.Statements[0].Affected != 1 {
		t.Fatalf("summary = %#v", res.Statements)
	}
}

// A later statement consumes an earlier statement's result by name through a
// ref: the program binding namespace end to end over the wire.
func TestExecuteStatementResultBinding(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := protocol.Program{
		Statements: []protocol.Statement{
			protocol.Read("base", protocol.Query{
				Nodes: map[string]protocol.Node{
					"r": {Kind: "rows", Scope: "r",
						Columns: []protocol.RowsColumn{{Name: "n", Type: "int64"}},
						Rows:    [][]any{{1}, {2}, {3}}},
					"o": {Kind: "order", Input: "r",
						Terms: []protocol.OrderTerm{{Expr: *protocol.Col("r", "n")}}},
				},
				Root: protocol.Root{Node: "o", Cardinality: "many"},
			}),
			// Reference "base" and keep only n > 1, ordered.
			protocol.Read("filtered", protocol.Query{
				Nodes: map[string]protocol.Node{
					"b": {Kind: "ref", Binding: "base", Scope: "b"},
					"f": {Kind: "filter", Input: "b",
						Predicate: protocol.Gt(protocol.Col("b", "n"), protocol.Lit(1))},
					"o": {Kind: "order", Input: "f",
						Terms: []protocol.OrderTerm{{Expr: *protocol.Col("b", "n")}}},
				},
				Root: protocol.Root{Node: "o", Cardinality: "many"},
			}),
		},
		Result: "filtered",
	}

	res, err := c.Execute(ctx, prog)
	if err != nil {
		t.Fatal(err)
	}
	rows, _ := res.Result.([]any)
	if len(rows) != 2 {
		t.Fatalf("result = %#v, want two rows (n>1)", res.Result)
	}
	// Summary carries both statements in order.
	if len(res.Statements) != 2 ||
		res.Statements[0].Name != "base" || res.Statements[0].Affected != 3 ||
		res.Statements[1].Name != "filtered" || res.Statements[1].Affected != 2 {
		t.Fatalf("summary = %#v", res.Statements)
	}
}

// A ref that points forward (at a not-yet-defined statement) is rejected: the
// backward-only rule falls out of registering each result only after binding.
func TestExecuteForwardReferenceRejected(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := protocol.Program{
		Statements: []protocol.Statement{
			protocol.Read("uses", protocol.Query{
				Nodes: map[string]protocol.Node{
					"r": {Kind: "ref", Binding: "later", Scope: "r"},
					"o": {Kind: "order", Input: "r",
						Terms: []protocol.OrderTerm{{Expr: *protocol.Col("r", "n")}}},
				},
				Root: protocol.Root{Node: "o", Cardinality: "many"},
			}),
			protocol.Read("later", oneRow("n", 1)),
		},
		Result: "uses",
	}
	_, err := c.Execute(ctx, prog)
	assertProblem(t, err, protocol.CodeInvalid, "unknown binding")
}

func TestExecuteMultiStatementNeedsResult(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := protocol.Program{Statements: []protocol.Statement{
		protocol.Read("a", oneRow("n", 1)),
		protocol.Read("b", oneRow("n", 2)),
	}}
	// MarshalProgram validates the envelope client-side before sending.
	_, err := c.Execute(ctx, prog)
	if err == nil {
		t.Fatal("multi-statement program without a result should be rejected")
	}
}

// assertProblem asserts err is an API problem with the given code and detail.
func assertProblem(t *testing.T, err error, code, detail string) {
	t.Helper()
	var ae *radclient.APIError
	if err == nil {
		t.Fatalf("want an error containing %q", detail)
	}
	if !errors.As(err, &ae) || ae.Problem.Code != code {
		t.Fatalf("err = %v, want code %q", err, code)
	}
	if detail != "" && !strings.Contains(ae.Problem.Detail, detail) {
		t.Fatalf("detail %q should mention %q", ae.Problem.Detail, detail)
	}
}
