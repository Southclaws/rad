package bind

import (
	"encoding/json"
	"fmt"
	"strconv"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func (b *binder) bindExpr(e lir.Expr) (bound.Expr, error) {
	switch x := e.(type) {
	case lir.Literal:
		// A typed NULL (a projected NULL carries its declared type) binds
		// directly; any other literal with no column context types itself
		// from its raw form.
		if x.Raw == nil && x.Kind != "" {
			if !x.Kind.Scalar() {
				return nil, reject.Inputf("planner: a NULL literal cannot be of type %s", x.Kind)
			}
			return bound.Literal{V: lir.Null(x.Kind.CatalogType())}, nil
		}
		v, err := inferLiteral(x.Raw)
		if err != nil {
			return nil, err
		}
		return bound.Literal{V: v}, nil

	case lir.Column:
		return b.resolveColumn(x)

	case lir.Unary:
		sub, err := b.bindExpr(x.X)
		if err != nil {
			return nil, err
		}
		switch x.Op {
		case lir.OpNot:
			if sub.Type().Kind != lir.KindBool {
				return nil, reject.Inputf("planner: not needs a boolean, got %s", sub.Type())
			}
		case lir.OpNegate:
			if !sub.Type().Kind.Numeric() {
				return nil, reject.Inputf("planner: cannot negate %s", sub.Type())
			}
		case lir.OpIsNull, lir.OpIsNotNull:
			// Any nullable value has a null to test — including a first
			// crossing's row. Arrays are empty, never NULL.
			if sub.Type().Kind == lir.KindArray {
				return nil, reject.Inputf("planner: %s is meaningless on an array — arrays are empty, never NULL", x.Op)
			}
		default:
			return nil, reject.Inputf("planner: unknown unary operator %q", x.Op)
		}
		return bound.NewUnary(x.Op, sub), nil

	case lir.Binary:
		return b.bindBinary(x)

	case lir.Cast:
		sub, err := b.bindExpr(x.X)
		if err != nil {
			return nil, err
		}
		if !x.To.Scalar() {
			return nil, reject.Inputf("planner: cannot cast to %s", x.To)
		}
		from := sub.Type().Kind
		ok := from == x.To ||
			(from == lir.KindInt64 && x.To == lir.KindFloat64) ||
			(from == lir.KindFloat64 && x.To == lir.KindInt64)
		if !ok {
			return nil, reject.Inputf("planner: cannot cast %s to %s", from, x.To)
		}
		return bound.NewCast(sub, x.To), nil

	case lir.Branch:
		return b.bindBranch(x)

	case lir.Exists:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		return bound.Exists{Rel: rel}, nil

	case lir.First:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		if !rel.Card().AtMostOne() && !bound.Ordered(rel) {
			return nil, reject.Inputf("planner: first over an unordered multi-row relation would make results depend on the access path — add an order or make the relation at-most-one")
		}
		if err := requireUniqueOutput(rel, "first crossing output"); err != nil {
			return nil, err
		}
		return bound.NewFirst(rel), nil

	case lir.Scalar:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		if !rel.Card().AtMostOne() {
			return nil, reject.Inputf("planner: scalar asserts at most one row, but the relation may produce more — aggregate it, slice it, or pin a unique key")
		}
		return bound.NewScalar(rel)

	case lir.Array:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		if !bound.Ordered(rel) {
			return nil, reject.Inputf("planner: array over an unordered relation would make observable collection order depend on the access path — add an order")
		}
		if err := requireUniqueOutput(rel, "array crossing output"); err != nil {
			return nil, err
		}
		return bound.NewArray(rel), nil

	case nil:
		return nil, reject.Inputf("planner: missing expression")
	default:
		return nil, reject.Inputf("planner: unknown expression node %T", e)
	}
}

// bindSubRel binds a crossing's relation: the current scope stack stays
// visible — that is how correlation binds — but scopes bound inside the
// sub-relation never leak out.
func (b *binder) bindSubRel(r lir.Relation) (bound.Relation, error) {
	if r == nil {
		return nil, reject.Inputf("planner: crossing needs a relation")
	}
	mark := len(b.scopes)
	rel, err := b.bindRel(r)
	if err != nil {
		return nil, err
	}
	b.scopes = b.scopes[:mark]
	return rel, nil
}

func (b *binder) resolveColumn(c lir.Column) (bound.SlotRef, error) {
	if c.Scope == "" {
		return bound.SlotRef{}, reject.Inputf("planner: column %q needs a scope qualifier", c.Name)
	}
	entry, ok := b.findScope(c.Scope, 0)
	if !ok {
		// A label that was bound somewhere but is not visible here was either
		// closed by a projection/aggregate boundary or belongs to another
		// sub-relation — the most common authoring mistake, worth naming.
		if b.labels[c.Scope] {
			return bound.SlotRef{}, reject.Fail(reject.ReasonUnknownScope, "planner: scope %q exists but is not visible here — a projection or aggregate closed it, or it belongs to a different sub-relation; label that node's output scope and reference its columns instead", c.Scope)
		}
		return bound.SlotRef{}, reject.Fail(reject.ReasonUnknownScope, "planner: unknown scope %q", c.Scope)
	}
	f, ok := entry.rel.Output().Lookup(c.Name)
	if !ok {
		return bound.SlotRef{}, reject.Fail(reject.ReasonUnknownColumn, "planner: scope %q has no column %q", c.Scope, c.Name)
	}
	return bound.SlotRef{Slot: f.Slot, Name: c.Scope + "." + c.Name, T: f.Type}, nil
}

// bindBinary binds both operands, coercing a raw literal against the other
// side's type — a JSON number becomes int64 or float64 by the column it
// meets, never by guessing.
func (b *binder) bindBinary(x lir.Binary) (bound.Expr, error) {
	switch x.Op {
	case lir.OpAnd, lir.OpOr:
		l, err := b.bindExpr(x.L)
		if err != nil {
			return nil, err
		}
		r, err := b.bindExpr(x.R)
		if err != nil {
			return nil, err
		}
		for _, side := range []bound.Expr{l, r} {
			if side.Type().Kind != lir.KindBool {
				return nil, reject.Inputf("planner: %s needs boolean operands, got %s", x.Op, side.Type())
			}
		}
		return bound.NewBinary(x.Op, l, r), nil

	case lir.OpEq, lir.OpNe, lir.OpLt, lir.OpLte, lir.OpGt, lir.OpGte:
		l, r, err := b.bindOperands(x.L, x.R)
		if err != nil {
			return nil, err
		}
		lk, rk := l.Type().Kind, r.Type().Kind
		if !lk.Scalar() || !rk.Scalar() {
			return nil, reject.Inputf("planner: cannot compare %s with %s", lk, rk)
		}
		if lk != rk {
			// No numeric widening in comparisons: an int64 column never
			// silently compares against a float literal.
			return nil, reject.Inputf("planner: cannot compare %s with %s", lk, rk)
		}
		return bound.NewBinary(x.Op, l, r), nil

	case lir.OpAdd, lir.OpSub, lir.OpMul, lir.OpDiv:
		l, r, err := b.bindOperands(x.L, x.R)
		if err != nil {
			return nil, err
		}
		if !l.Type().Kind.Numeric() || !r.Type().Kind.Numeric() {
			return nil, reject.Inputf("planner: %s needs numeric operands, got %s and %s", x.Op, l.Type(), r.Type())
		}
		return bound.NewBinary(x.Op, l, r), nil

	default:
		return nil, reject.Inputf("planner: unknown binary operator %q", x.Op)
	}
}

// bindBranch binds ordered lazy branching. Every when must be boolean;
// every then and the else must share one scalar kind — no implicit
// widening, a frontend inserts explicit casts. The whole subtree must be
// crossing-free: the executor attaches crossings eagerly per row before
// expression evaluation, which would evaluate a crossing in an arm the
// branch never selects and make its errors observable, violating the
// laziness contract.
func (b *binder) bindBranch(x lir.Branch) (bound.Expr, error) {
	if len(x.Arms) == 0 {
		return nil, reject.Inputf("planner: branch needs at least one arm")
	}
	if x.Else == nil {
		return nil, reject.Inputf("planner: branch needs an else — LIR has no implicit NULL default; supply a typed NULL explicitly")
	}
	for i, arm := range x.Arms {
		if err := rejectBranchCrossings(arm.When, fmt.Sprintf("arm %d's when", i+1)); err != nil {
			return nil, err
		}
		if err := rejectBranchCrossings(arm.Then, fmt.Sprintf("arm %d's then", i+1)); err != nil {
			return nil, err
		}
	}
	if err := rejectBranchCrossings(x.Else, "the else"); err != nil {
		return nil, err
	}

	arms := make([]bound.BranchArm, len(x.Arms))
	var kind lir.Kind
	for i, arm := range x.Arms {
		when, err := b.bindExpr(arm.When)
		if err != nil {
			return nil, err
		}
		if when.Type().Kind != lir.KindBool {
			return nil, reject.Inputf("planner: branch arm %d when must be boolean, got %s", i+1, when.Type())
		}
		then, err := b.bindExpr(arm.Then)
		if err != nil {
			return nil, err
		}
		tk := then.Type().Kind
		if !tk.Scalar() {
			return nil, reject.Inputf("planner: branch arm %d result must be a scalar, got %s", i+1, then.Type())
		}
		if i == 0 {
			kind = tk
		} else if tk != kind {
			return nil, reject.Inputf("planner: branch arm %d result is %s but arm 1 result is %s — every arm and the else must share one scalar kind; cast the arm explicitly", i+1, tk, kind)
		}
		arms[i] = bound.BranchArm{When: when, Then: then}
	}
	els, err := b.bindExpr(x.Else)
	if err != nil {
		return nil, err
	}
	if !els.Type().Kind.Scalar() {
		return nil, reject.Inputf("planner: branch else must be a scalar, got %s", els.Type())
	}
	if els.Type().Kind != kind {
		return nil, reject.Inputf("planner: branch else is %s but arm 1 result is %s — every arm and the else must share one scalar kind; cast it explicitly", els.Type().Kind, kind)
	}
	return bound.NewBranch(arms, els), nil
}

// rejectBranchCrossings walks one branch component for crossings, which are
// illegal anywhere under a branch: the executor attaches every crossing
// eagerly per row before expression evaluation, so a crossing in an arm the
// branch never selects would still run.
func rejectBranchCrossings(e lir.Expr, where string) error {
	var kind string
	switch e.(type) {
	case lir.Exists:
		kind = "exists"
	case lir.First:
		kind = "first"
	case lir.Scalar:
		kind = "scalar"
	case lir.Array:
		kind = "array"
	default:
		for _, child := range lirExpressionChildren(e).expressions {
			if err := rejectBranchCrossings(child, where); err != nil {
				return err
			}
		}
		return nil
	}
	return reject.Inputf("planner: branch cannot contain a crossing (%s in %s) — crossings evaluate per row before the branch selects an arm, so one in a never-selected arm would still run", kind, where)
}

// bindOperands binds a comparison or arithmetic pair. When exactly one side
// is a raw literal, the other side binds first and the literal coerces to
// its type; a NULL literal adopts it.
func (b *binder) bindOperands(l, r lir.Expr) (bound.Expr, bound.Expr, error) {
	llit, lIsLit := l.(lir.Literal)
	rlit, rIsLit := r.(lir.Literal)

	switch {
	case lIsLit && !rIsLit:
		br, err := b.bindExpr(r)
		if err != nil {
			return nil, nil, err
		}
		bl, err := coerceLiteral(llit.Raw, br.Type())
		if err != nil {
			return nil, nil, err
		}
		return bl, br, nil
	case rIsLit && !lIsLit:
		bl, err := b.bindExpr(l)
		if err != nil {
			return nil, nil, err
		}
		br, err := coerceLiteral(rlit.Raw, bl.Type())
		if err != nil {
			return nil, nil, err
		}
		return bl, br, nil
	default:
		bl, err := b.bindExpr(l)
		if err != nil {
			return nil, nil, err
		}
		br, err := b.bindExpr(r)
		if err != nil {
			return nil, nil, err
		}
		return bl, br, nil
	}
}

// coerceLiteral types a raw wire scalar against a known context type.
func coerceLiteral(raw any, want lir.Type) (bound.Literal, error) {
	if !want.Kind.Scalar() {
		return bound.Literal{}, reject.Inputf("planner: a literal cannot be a %s", want.Kind)
	}
	ct := want.Kind.CatalogType()
	if raw == nil {
		return bound.Literal{V: lir.Null(ct)}, nil
	}

	fail := func() (bound.Literal, error) {
		return bound.Literal{}, reject.Inputf("planner: expected a %s value, got %T (%v)", ct, raw, raw)
	}
	switch v := raw.(type) {
	case string:
		if ct != model.TypeText {
			return fail()
		}
		return bound.Literal{V: lir.Text(v)}, nil
	case bool:
		if ct != model.TypeBool {
			return fail()
		}
		return bound.Literal{V: lir.Bool(v)}, nil
	case json.Number:
		switch ct {
		case model.TypeInt64:
			n, err := strconv.ParseInt(v.String(), 10, 64)
			if err != nil {
				return bound.Literal{}, reject.Inputf("planner: expected an int64 value, got %q — cast the column to float64 to compare against fractional values", v.String())
			}
			return bound.Literal{V: lir.Int64(n)}, nil
		case model.TypeFloat64:
			f, err := v.Float64()
			if err != nil {
				return bound.Literal{}, reject.Inputf("planner: expected a float64 value, got %q", v.String())
			}
			return bound.Literal{V: lir.Float64(f)}, nil
		}
		return fail()
	case int:
		return coerceGoInt(int64(v), ct)
	case int64:
		return coerceGoInt(v, ct)
	case float64:
		if ct == model.TypeInt64 {
			return bound.Literal{}, reject.Inputf("planner: expected an int64 value, got %v — cast the column to float64 to compare against fractional values", v)
		}
		if ct != model.TypeFloat64 {
			return fail()
		}
		return bound.Literal{V: lir.Float64(v)}, nil
	}
	return fail()
}

func coerceGoInt(v int64, ct model.Type) (bound.Literal, error) {
	switch ct {
	case model.TypeInt64:
		return bound.Literal{V: lir.Int64(v)}, nil
	case model.TypeFloat64:
		return bound.Literal{V: lir.Float64(float64(v))}, nil
	}
	return bound.Literal{}, reject.Inputf("planner: expected a %s value, got integer %d", ct, v)
}

// inferLiteral types a literal with no column context — a literal compared
// against another literal, or used arithmetically. Integers stay int64;
// a bare NULL has no type to adopt and is rejected.
func inferLiteral(raw any) (lir.Value, error) {
	switch v := raw.(type) {
	case nil:
		return lir.Value{}, reject.Inputf("planner: a bare NULL literal needs a typed context")
	case string:
		return lir.Text(v), nil
	case bool:
		return lir.Bool(v), nil
	case int:
		return lir.Int64(int64(v)), nil
	case int64:
		return lir.Int64(v), nil
	case float64:
		return lir.Float64(v), nil
	case json.Number:
		if n, err := strconv.ParseInt(v.String(), 10, 64); err == nil {
			return lir.Int64(n), nil
		}
		if f, err := v.Float64(); err == nil {
			return lir.Float64(f), nil
		}
		return lir.Value{}, reject.Inputf("planner: malformed number %q", v.String())
	}
	return lir.Value{}, reject.Inputf("planner: unsupported literal %T", raw)
}
