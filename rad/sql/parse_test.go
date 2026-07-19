package sql

import (
	"testing"

	"github.com/pgplex/pgparser/nodes"
	"github.com/pgplex/pgparser/parser"
)

func TestParamNumbersPreserved(t *testing.T) {
	list, err := parser.Parse(`SELECT a FROM t WHERE b = $2 AND c = $1`)
	if err != nil {
		t.Fatal(err)
	}
	sel := list.Items[0].(*nodes.SelectStmt)
	be := sel.WhereClause.(*nodes.BoolExpr)
	want := []int{2, 1}
	for i, arg := range be.Args.Items {
		pr := arg.(*nodes.A_Expr).Rexpr.(*nodes.ParamRef)
		if pr.Number != want[i] {
			t.Fatalf("param %d: got number %d, want %d", i, pr.Number, want[i])
		}
	}
}
