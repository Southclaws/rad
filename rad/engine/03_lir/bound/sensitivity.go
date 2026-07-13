package bound

// Plan-choice sensitivity: can two valid physical plans for a relation
// commit different legal bags? This is never about executor nondeterminism
// (there is none — replay is deterministic); it is about logical
// arbitrariness meeting plan choice. A bare scan is path-independent as a
// bag: its encounter order is arbitrary but unobserved. A slice converts
// arbitrary order into arbitrary membership. First and Array crossings
// materialise a row choice or an order into the datum itself.
//
// The classifier is deliberately conservative — a slice above a proven
// total order is not actually sensitive, but proving that costs more than
// it saves while every binding is materialised once anyway. Refinement is
// an optimisation for the day occurrence plans diverge.

// PlanSensitive reports whether rel's committed value may depend on which
// valid physical plan evaluates it.
func PlanSensitive(rel Relation) bool {
	switch n := rel.(type) {
	case *Slice:
		return true
	case *Ref:
		// A reference observes an already-committed value: whatever
		// sensitivity the binding's body had was resolved at its own
		// commitment. The occurrence itself is deterministic.
		return false
	case *Filter:
		if exprSensitive(n.Pred) {
			return true
		}
	case *Project:
		for _, f := range n.Fields {
			if exprSensitive(f.Expr) {
				return true
			}
		}
	case *Join:
		if exprSensitive(n.On) {
			return true
		}
	case *Aggregate:
		for _, g := range n.Groups {
			if exprSensitive(g.Expr) {
				return true
			}
		}
		for _, t := range n.Terms {
			if t.Arg != nil && exprSensitive(t.Arg) {
				return true
			}
		}
	case *Order:
		for _, t := range n.Terms {
			if exprSensitive(t.Expr) {
				return true
			}
		}
	}
	for _, in := range rel.Inputs() {
		if PlanSensitive(in) {
			return true
		}
	}
	return false
}

// exprSensitive walks an expression for crossings whose materialisation
// depends on plan choice: First picks a row (tie-sensitive unless the body
// is statically at-most-one — conservatively flagged), Array captures an
// order into the datum. Exists and Scalar carry no order and recurse into
// their bodies.
func exprSensitive(e Expr) bool {
	switch x := e.(type) {
	case First:
		return true
	case Array:
		return true
	case Exists:
		return PlanSensitive(x.Rel)
	case Scalar:
		return PlanSensitive(x.Rel)
	case Unary:
		return exprSensitive(x.X)
	case Binary:
		return exprSensitive(x.L) || exprSensitive(x.R)
	case Cast:
		return exprSensitive(x.X)
	case Call:
		for _, a := range x.Args {
			if exprSensitive(a) {
				return true
			}
		}
	}
	return false
}
