package bound

// WalkExpr visits an expression tree. Crossing relations are leaves because
// they belong to the relational tree rather than the expression tree.
func WalkExpr(expr Expr, visit func(Expr)) {
	walkExpr(expr, visit, nil)
}

// WalkRelation visits a relation tree, its expressions, and relations reached
// through cardinality crossings.
func WalkRelation(rel Relation, visitRel func(Relation), visitExpr func(Expr)) {
	if rel == nil {
		return
	}
	if visitRel != nil {
		visitRel(rel)
	}
	visitRelationExprs(rel, func(expr Expr) {
		walkExpr(expr, visitExpr, func(crossing Relation) {
			WalkRelation(crossing, visitRel, visitExpr)
		})
	})
	for _, input := range rel.Inputs() {
		WalkRelation(input, visitRel, visitExpr)
	}
}

func walkExpr(expr Expr, visit func(Expr), visitRel func(Relation)) {
	if expr == nil {
		return
	}
	if visit != nil {
		visit(expr)
	}
	switch expr := expr.(type) {
	case Unary:
		walkExpr(expr.X, visit, visitRel)
	case Binary:
		walkExpr(expr.L, visit, visitRel)
		walkExpr(expr.R, visit, visitRel)
	case Cast:
		walkExpr(expr.X, visit, visitRel)
	case Exists:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	case First:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	case Scalar:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	case Array:
		if visitRel != nil {
			visitRel(expr.Rel)
		}
	}
}

func visitRelationExprs(rel Relation, visit func(Expr)) {
	switch rel := rel.(type) {
	case *Filter:
		visit(rel.Pred)
	case *Project:
		for _, field := range rel.Fields {
			visit(field.Expr)
		}
	case *Join:
		visit(rel.On)
	case *Aggregate:
		for _, group := range rel.Groups {
			visit(group.Expr)
		}
		for _, term := range rel.Terms {
			visit(term.Arg)
		}
	case *Order:
		for _, term := range rel.Terms {
			visit(term.Expr)
		}
	}
}
