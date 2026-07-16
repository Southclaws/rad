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
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// oneRow builds a one-row int64 constant relation — a self-contained query
// body needing no fixture.
func oneRow(col string, v int) lirwire.Query {
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"r": lirwire.Rows("r", []lirwire.RowsColumn{{Name: col, Type: "int64"}}, [][]lirwire.Cell{{mustValue(v)}}),
			"o": lirwire.Order("r", []lirwire.OrderTerm{{Expr: lirwire.Col("r", col)}}),
		},
		Root: lirwire.Root{Node: "o", Cardinality: "many"},
	}
}

// A single-statement query program returns that statement's datum with no
// result selector.
func TestExecuteSingleQuery(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	res, err := c.Execute(ctx, pirwire.Prog("", pirwire.Query("result", relBytes(oneRow("n", 7)))))
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

	prog := pirwire.Prog("filtered",
		pirwire.Query("base", relBytes(lirwire.Query{
			Nodes: map[string]lirwire.Node{
				"r": lirwire.Rows("r", []lirwire.RowsColumn{{Name: "n", Type: "int64"}}, [][]lirwire.Cell{{mustValue(1)}, {mustValue(2)}, {mustValue(3)}}),
				"o": lirwire.Order("r", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "n")}}),
			},
			Root: lirwire.Root{Node: "o", Cardinality: "many"},
		})),
		// Reference "base" and keep only n > 1, ordered.
		pirwire.Query("filtered", relBytes(lirwire.Query{
			Nodes: map[string]lirwire.Node{
				"b": lirwire.Ref("base", "b"),
				"f": lirwire.Filter("b", lirwire.Binary("gt", lirwire.Col("b", "n"), lirwire.LitOf(1))),
				"o": lirwire.Order("f", []lirwire.OrderTerm{{Expr: lirwire.Col("b", "n")}}),
			},
			Root: lirwire.Root{Node: "o", Cardinality: "many"},
		})),
	)

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

	prog := pirwire.Prog("uses",
		pirwire.Query("uses", relBytes(lirwire.Query{
			Nodes: map[string]lirwire.Node{
				"r": lirwire.Ref("later", "r"),
				"o": lirwire.Order("r", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "n")}}),
			},
			Root: lirwire.Root{Node: "o", Cardinality: "many"},
		})),
		pirwire.Query("later", relBytes(oneRow("n", 1))),
	)
	_, err := c.Execute(ctx, prog)
	assertProblem(t, err, protocol.CodeInvalid, "unknown binding")
}

// A statement-local binding may not shadow any program statement name, even
// one defined by a later statement — the whole program's namespace is
// collision-free regardless of order.
func TestExecuteLocalBindingCannotShadowStatement(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	// Statement "first" has a local binding named "second"; a later
	// statement is also named "second".
	first := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"base": lirwire.Rows("b", []lirwire.RowsColumn{{Name: "n", Type: "int64"}}, [][]lirwire.Cell{{mustValue(1)}}),
			"u":    lirwire.Ref("second", "u"),
			"o":    lirwire.Order("u", []lirwire.OrderTerm{{Expr: lirwire.Col("u", "n")}}),
		},
		Bindings: map[string]lirwire.Binding{"second": {Node: "base"}},
		Root:     lirwire.Root{Node: "o", Cardinality: "many"},
	}
	prog := pirwire.Prog("second",
		pirwire.Query("first", relBytes(first)),
		pirwire.Query("second", relBytes(oneRow("n", 2))),
	)
	_, err := c.Execute(ctx, prog)
	assertProblem(t, err, protocol.CodeInvalid, "shadows a statement name")
}

func TestExecuteMultiStatementNeedsResult(t *testing.T) {
	c := migrated(t)
	ctx := context.Background()

	prog := pirwire.Prog("",
		pirwire.Query("a", relBytes(oneRow("n", 1))),
		pirwire.Query("b", relBytes(oneRow("n", 2))),
	)
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
