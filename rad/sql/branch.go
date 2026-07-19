package sql

import (
	"fmt"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// Conditional expressions lower onto LIR's branch: ordered arms, strict
// first-TRUE matching, mandatory else. The engine requires every arm to
// share one scalar kind, so lowering runs two passes: discover the unified
// result type from the arms that carry one, then re-lower every arm against
// it — coercing literals and parameters, typing bare NULLs, and inserting
// int64→float64 casts where the arms mix numerics.

// armSource is one prospective (when, then) pair before result-type
// unification. A nil when marks the else arm.
type armSource struct {
	when nodes.Node
	then nodes.Node
}

func (c *cc) lowerCase(e *env, ce *nodes.CaseExpr) (lirwire.Expr, exprType, error) {
	if ce.Args == nil || len(ce.Args.Items) == 0 {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("CASE without WHEN arms")
	}
	arms := make([]armSource, 0, len(ce.Args.Items)+1)
	for _, item := range ce.Args.Items {
		cw, ok := item.(*nodes.CaseWhen)
		if !ok {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("unexpected CASE arm %T", item)
		}
		arms = append(arms, armSource{when: cw.Expr, then: cw.Result})
	}
	arms = append(arms, armSource{then: ce.Defresult})

	// The discriminant form compares the argument against each WHEN value;
	// the comparison is built per arm (duplicate evaluation of a pure
	// expression, which the engine may later deduplicate).
	whenOf := func(when nodes.Node) (lirwire.Expr, error) {
		if ce.Arg == nil {
			pred, _, err := c.lowerExpr(e, when, &boolType)
			return pred, err
		}
		le, lt, re, rt, err := c.lowerOperands(e, ce.Arg, when)
		if err != nil {
			return lirwire.Expr{}, err
		}
		le, _, re, _, err = alignComparison(le, lt, re, rt)
		if err != nil {
			return lirwire.Expr{}, err
		}
		return lirwire.Binary("eq", le, re), nil
	}
	return c.lowerBranch(e, arms, whenOf)
}

func (c *cc) lowerCoalesce(e *env, co *nodes.CoalesceExpr) (lirwire.Expr, exprType, error) {
	if co.Args == nil || len(co.Args.Items) == 0 {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("COALESCE without arguments")
	}
	items := co.Args.Items
	if len(items) == 1 {
		return c.lowerExpr(e, items[0], nil)
	}
	arms := make([]armSource, 0, len(items))
	for _, item := range items[:len(items)-1] {
		arms = append(arms, armSource{when: item, then: item})
	}
	arms = append(arms, armSource{then: items[len(items)-1]})
	whenOf := func(when nodes.Node) (lirwire.Expr, error) {
		arg, _, err := c.lowerExpr(e, when, nil)
		if err != nil {
			return lirwire.Expr{}, err
		}
		return lirwire.Unary("is_not_null", arg), nil
	}
	return c.lowerBranch(e, arms, whenOf)
}

func (c *cc) lowerNullIf(e *env, a *nodes.A_Expr) (lirwire.Expr, exprType, error) {
	le, lt, re, rt, err := c.lowerOperands(e, a.Lexpr, a.Rexpr)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	cle, clt, cre, _, err := alignComparison(le, lt, re, rt)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	arm := lirwire.Arm(lirwire.Binary("eq", cle, cre), lirwire.Lit(lirwire.Null(clt.scalar)))
	out := lirwire.Branch([]lirwire.BranchArm{arm}, cle)
	t := clt
	t.nullable = true
	return out, t, nil
}

// lowerBranch performs result-type unification over the arm sources and
// assembles the branch expression. whenOf lowers one arm's predicate.
func (c *cc) lowerBranch(e *env, arms []armSource, whenOf func(nodes.Node) (lirwire.Expr, error)) (lirwire.Expr, exprType, error) {
	// First pass: discover the unified result type from arms that have one
	// of their own (skipping bare NULLs and other context-typed leaves).
	unified := exprType{}
	found := false
	mixedNumeric := false
	for _, arm := range arms {
		if arm.then == nil || flexible(arm.then) {
			// NULLs, literals-in-waiting, and parameters take their type
			// from the other arms, not the reverse.
			if _, isConst := arm.then.(*nodes.A_Const); !isConst || isNullConst(arm.then) {
				continue
			}
		}
		probe := newCC(c.schema, &paramTracker{}, nil)
		probe.ctes = c.ctes
		probe.rec = c.rec
		_, t, err := probe.lowerExpr(e, arm.then, nil)
		if err != nil {
			continue
		}
		if !found {
			unified = t
			found = true
			continue
		}
		if t.scalar != unified.scalar {
			if numeric(t.scalar) && numeric(unified.scalar) {
				mixedNumeric = true
				unified.scalar = lirwire.ScalarTypeFloat64
			} else {
				return lirwire.Expr{}, exprType{}, fmt.Errorf("CASE arms have mismatched types %s and %s", unified.scalar, t.scalar)
			}
		}
		if t.format != unified.format {
			unified.format = ""
		}
	}
	if !found {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("cannot determine CASE result type")
	}
	_ = mixedNumeric

	// Second pass: lower every arm against the unified type.
	lowerThen := func(n nodes.Node) (lirwire.Expr, exprType, error) {
		if n == nil {
			t := unified
			t.nullable = true
			return lirwire.Lit(lirwire.Null(unified.scalar)), t, nil
		}
		expr, t, err := c.lowerExpr(e, n, &unified)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		if t.scalar != unified.scalar {
			if numeric(t.scalar) && numeric(unified.scalar) {
				expr = lirwire.Cast(expr, unified.scalar)
				t.scalar = unified.scalar
			} else {
				return lirwire.Expr{}, exprType{}, fmt.Errorf("CASE arm has type %s, expected %s", t.scalar, unified.scalar)
			}
		}
		return expr, t, nil
	}

	nullable := false
	var built []lirwire.BranchArm
	var elseExpr lirwire.Expr
	for i, arm := range arms {
		then, t, err := lowerThen(arm.then)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		nullable = nullable || t.nullable
		if i == len(arms)-1 {
			elseExpr = then
			break
		}
		when, err := whenOf(arm.when)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		built = append(built, lirwire.Arm(when, then))
	}
	result := unified
	result.nullable = nullable
	return lirwire.Branch(built, elseExpr), result, nil
}

func isNullConst(n nodes.Node) bool {
	ac, ok := n.(*nodes.A_Const)
	return ok && ac.Isnull
}
