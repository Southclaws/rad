package generative

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Features walks an unbound query and records which constructs and compositions
// it contains — a structure-aware coverage signal for auditing a generator's
// reach.
func Features(q lir.Query) map[string]bool {
	f := map[string]bool{}
	walkRelFeat(q.Root, f, false)
	for _, b := range q.Bindings {
		walkRelFeat(b, f, false)
	}
	return f
}

func walkRelFeat(r lir.Relation, f map[string]bool, inCross bool) {
	switch n := r.(type) {
	case lir.Scan:
		f["scan"] = true
	case lir.Rows:
		f["rows"] = true
	case lir.Ref:
		f["ref_binding"] = true
	case lir.Filter:
		f["filter"] = true
		walkExprFeat(n.Pred, f, inCross)
		walkRelFeat(n.Input, f, inCross)
	case lir.Project:
		f["project"] = true
		for _, fld := range n.Fields {
			walkExprFeat(fld.Expr, f, inCross)
		}
		walkRelFeat(n.Input, f, inCross)
	case lir.Join:
		if n.Kind == lir.LeftJoin {
			f["join_left"] = true
		} else {
			f["join_inner"] = true
		}
		walkExprFeat(n.On, f, inCross)
		walkRelFeat(n.Left, f, inCross)
		walkRelFeat(n.Right, f, inCross)
	case lir.Concatenate:
		f["concatenate"] = true
		for _, in := range n.Inputs {
			walkRelFeat(in, f, inCross)
		}
	case lir.Intersect:
		f["intersect"] = true
		walkRelFeat(n.Left, f, inCross)
		walkRelFeat(n.Right, f, inCross)
	case lir.Except:
		f["except"] = true
		walkRelFeat(n.Left, f, inCross)
		walkRelFeat(n.Right, f, inCross)
	case lir.Order:
		f["order"] = true
		for _, t := range n.Terms {
			walkExprFeat(t.Expr, f, inCross)
		}
		walkRelFeat(n.Input, f, inCross)
	case lir.Slice:
		f["slice"] = true
		walkRelFeat(n.Input, f, inCross)
	case lir.Aggregate:
		f["aggregate"] = true
		if len(n.Groups) == 0 {
			f["global_aggregate"] = true
		} else {
			f["group_by"] = true
		}
		for _, g := range n.Groups {
			walkExprFeat(g.Expr, f, inCross)
		}
		for _, t := range n.Terms {
			if t.Arg != nil {
				walkExprFeat(t.Arg, f, inCross)
			}
		}
		walkRelFeat(n.Input, f, inCross)
	}
}

func walkExprFeat(e lir.Expr, f map[string]bool, inCross bool) {
	switch x := e.(type) {
	case lir.Unary:
		switch x.Op {
		case lir.OpIsNull, lir.OpIsNotNull:
			f["is_null"] = true
		case lir.OpNot:
			f["not"] = true
		case lir.OpNegate:
			f["arithmetic"] = true
		}
		walkExprFeat(x.X, f, inCross)
	case lir.Binary:
		switch x.Op {
		case lir.OpAdd, lir.OpSub, lir.OpMul, lir.OpDiv:
			f["arithmetic"] = true
		case lir.OpAnd, lir.OpOr:
			f["and_or"] = true
		}
		walkExprFeat(x.L, f, inCross)
		walkExprFeat(x.R, f, inCross)
	case lir.Cast:
		f["cast"] = true
		walkExprFeat(x.X, f, inCross)
	case lir.Branch:
		f["branch"] = true
		for _, arm := range x.Arms {
			walkExprFeat(arm.When, f, inCross)
			walkExprFeat(arm.Then, f, inCross)
		}
		walkExprFeat(x.Else, f, inCross)
	case lir.Exists:
		crossFeat(x.Rel, f, "exists", inCross)
	case lir.First:
		crossFeat(x.Rel, f, "first", inCross)
	case lir.Scalar:
		crossFeat(x.Rel, f, "scalar", inCross)
	case lir.Array:
		crossFeat(x.Rel, f, "array", inCross)
	}
}

func crossFeat(rel lir.Relation, f map[string]bool, kind string, inCross bool) {
	f["crossing"] = true
	f[kind] = true
	if inCross {
		f["nested_crossing"] = true
	}
	if relContains(rel, func(r lir.Relation) bool { _, ok := r.(lir.Aggregate); return ok }) {
		f["correlated_aggregate"] = true
	}
	if relContains(rel, func(r lir.Relation) bool { _, ok := r.(lir.Join); return ok }) {
		f["crossing_over_join"] = true
	}
	walkRelFeat(rel, f, true)
}

// relContains reports whether any relation node in the tree (not descending
// into crossing sub-expressions) satisfies pred.
func relContains(r lir.Relation, pred func(lir.Relation) bool) bool {
	if pred(r) {
		return true
	}
	switch n := r.(type) {
	case lir.Filter:
		return relContains(n.Input, pred)
	case lir.Project:
		return relContains(n.Input, pred)
	case lir.Order:
		return relContains(n.Input, pred)
	case lir.Slice:
		return relContains(n.Input, pred)
	case lir.Aggregate:
		return relContains(n.Input, pred)
	case lir.Join:
		return relContains(n.Left, pred) || relContains(n.Right, pred)
	case lir.Concatenate:
		for _, in := range n.Inputs {
			if relContains(in, pred) {
				return true
			}
		}
	case lir.Intersect:
		return relContains(n.Left, pred) || relContains(n.Right, pred)
	case lir.Except:
		return relContains(n.Left, pred) || relContains(n.Right, pred)
	}
	return false
}
