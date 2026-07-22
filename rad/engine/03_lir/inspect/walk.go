package inspect

import "github.com/Southclaws/rad/rad/engine/03_lir/bound"

// WalkExpr visits an expression tree. Crossing relations are leaves because
// they belong to the relational tree rather than the expression tree.
func WalkExpr(expr bound.Expr, visit func(bound.Expr)) {
	walkExpr(expr, visit, nil)
}

// WalkRelation visits a relation tree, its expressions, and relations reached
// through cardinality crossings.
func WalkRelation(rel bound.Relation, visitRel func(bound.Relation), visitExpr func(bound.Expr)) {
	if rel == nil {
		return
	}
	if visitRel != nil {
		visitRel(rel)
	}
	visitRelationExprs(rel, func(expr bound.Expr) {
		walkExpr(expr, visitExpr, func(crossing bound.Relation) {
			WalkRelation(crossing, visitRel, visitExpr)
		})
	})
	for _, input := range rel.Inputs() {
		WalkRelation(input, visitRel, visitExpr)
	}
}

func walkExpr(expr bound.Expr, visit func(bound.Expr), visitRel func(bound.Relation)) {
	if expr == nil {
		return
	}
	if visit != nil {
		visit(expr)
	}
	switch expr := expr.(type) {
	case bound.Unary:
		walkExpr(expr.X, visit, visitRel)
	case bound.Binary:
		walkExpr(expr.L, visit, visitRel)
		walkExpr(expr.R, visit, visitRel)
	case bound.Cast:
		walkExpr(expr.X, visit, visitRel)
	case bound.Branch:
		for _, arm := range expr.Arms {
			walkExpr(arm.When, visit, visitRel)
			walkExpr(arm.Then, visit, visitRel)
		}
		walkExpr(expr.Else, visit, visitRel)
	case bound.TextMatch:
		walkExpr(expr.Value, visit, visitRel)
	case bound.Exists:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	case bound.First:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	case bound.Scalar:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	case bound.Array:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	}
}

func visitRelationExprs(rel bound.Relation, visit func(bound.Expr)) {
	switch rel := rel.(type) {
	case *bound.Filter:
		visit(rel.Pred)
	case *bound.Project:
		for _, field := range rel.Fields {
			visit(field.Expr)
		}
	case *bound.Join:
		visit(rel.On)
	case *bound.Aggregate:
		for _, group := range rel.Groups {
			visit(group.Expr)
		}
		for _, term := range rel.Terms {
			visit(term.Arg)
		}
	case *bound.Order:
		for _, term := range rel.Terms {
			visit(term.Expr)
		}
	}
}
