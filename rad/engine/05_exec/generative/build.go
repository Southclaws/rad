package generative

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Unbound-LIR construction shorthands used throughout query synthesis.

func qcol(scope, name string) lir.Column { return lir.Column{Scope: scope, Name: name} }
func qlit(v any) lir.Literal             { return lir.Literal{Raw: v} }
func qeq(l, r lir.Expr) lir.Expr         { return lir.Binary{Op: lir.OpEq, L: l, R: r} }
func qand(l, r lir.Expr) lir.Expr        { return lir.Binary{Op: lir.OpAnd, L: l, R: r} }
func qscan(tbl, scope string) lir.Scan   { return lir.Scan{Table: tbl, Scope: scope} }
func qfilter(in lir.Relation, p lir.Expr) lir.Filter {
	return lir.Filter{Input: in, Pred: p}
}

// many wraps a relation as a bag query with a constant-true order so the root
// carries a total (if arbitrary) order.
func many(root lir.Relation) lir.Query {
	return lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: root,
		Terms: []lir.OrderTerm{{Expr: qlit(true)}},
	}}
}
