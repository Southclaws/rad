package bind

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

type lirNodeChildren struct {
	relations   []lir.Relation
	expressions []lir.Expr
}

func lirRelationChildren(r lir.Relation) lirNodeChildren {
	switch n := r.(type) {
	case lir.Scan, lir.Rows, lir.Ref, lir.RecursiveRef, nil:
		return lirNodeChildren{}
	case lir.Filter:
		return lirNodeChildren{relations: []lir.Relation{n.Input}, expressions: []lir.Expr{n.Pred}}
	case lir.Project:
		expressions := make([]lir.Expr, len(n.Fields))
		for i, field := range n.Fields {
			expressions[i] = field.Expr
		}
		return lirNodeChildren{relations: []lir.Relation{n.Input}, expressions: expressions}
	case lir.Join:
		return lirNodeChildren{relations: []lir.Relation{n.Left, n.Right}, expressions: []lir.Expr{n.On}}
	case lir.Aggregate:
		expressions := make([]lir.Expr, 0, len(n.Groups)+len(n.Terms))
		for _, group := range n.Groups {
			expressions = append(expressions, group.Expr)
		}
		for _, term := range n.Terms {
			if term.Arg != nil {
				expressions = append(expressions, term.Arg)
			}
		}
		return lirNodeChildren{relations: []lir.Relation{n.Input}, expressions: expressions}
	case lir.Order:
		expressions := make([]lir.Expr, len(n.Terms))
		for i, term := range n.Terms {
			expressions[i] = term.Expr
		}
		return lirNodeChildren{relations: []lir.Relation{n.Input}, expressions: expressions}
	case lir.Slice:
		return lirNodeChildren{relations: []lir.Relation{n.Input}}
	case lir.Recursive:
		return lirNodeChildren{relations: []lir.Relation{n.Anchor, n.Step}}
	case lir.Distinct:
		return lirNodeChildren{relations: []lir.Relation{n.Input}}
	default:
		panic("planner: unknown LIR relation")
	}
}

func lirExpressionChildren(e lir.Expr) lirNodeChildren {
	switch x := e.(type) {
	case lir.Literal, lir.Column, nil:
		return lirNodeChildren{}
	case lir.Unary:
		return lirNodeChildren{expressions: []lir.Expr{x.X}}
	case lir.Binary:
		return lirNodeChildren{expressions: []lir.Expr{x.L, x.R}}
	case lir.Cast:
		return lirNodeChildren{expressions: []lir.Expr{x.X}}
	case lir.Exists:
		return lirNodeChildren{relations: []lir.Relation{x.Rel}}
	case lir.First:
		return lirNodeChildren{relations: []lir.Relation{x.Rel}}
	case lir.Scalar:
		return lirNodeChildren{relations: []lir.Relation{x.Rel}}
	case lir.Array:
		return lirNodeChildren{relations: []lir.Relation{x.Rel}}
	default:
		panic("planner: unknown LIR expression")
	}
}

func walkLIR(
	r lir.Relation,
	visitRelation func(lir.Relation) error,
	visitExpression func(lir.Expr) error,
) error {
	var walkRelation func(lir.Relation) error
	var walkExpression func(lir.Expr) error

	walkRelation = func(r lir.Relation) error {
		if visitRelation != nil {
			if err := visitRelation(r); err != nil {
				return err
			}
		}
		children := lirRelationChildren(r)
		for _, child := range children.relations {
			if err := walkRelation(child); err != nil {
				return err
			}
		}
		for _, child := range children.expressions {
			if err := walkExpression(child); err != nil {
				return err
			}
		}
		return nil
	}

	walkExpression = func(e lir.Expr) error {
		if visitExpression != nil {
			if err := visitExpression(e); err != nil {
				return err
			}
		}
		children := lirExpressionChildren(e)
		for _, child := range children.expressions {
			if err := walkExpression(child); err != nil {
				return err
			}
		}
		for _, child := range children.relations {
			if err := walkRelation(child); err != nil {
				return err
			}
		}
		return nil
	}

	return walkRelation(r)
}
