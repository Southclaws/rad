package planner

import (
	"encoding/json"
	"fmt"
	"strconv"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
)

func (b *binder) bindExpr(e qir.Expr) (bound.Expr, error) {
	switch x := e.(type) {
	case qir.Literal:
		// A literal with no column context types itself from its raw form.
		v, err := inferLiteral(x.Raw)
		if err != nil {
			return nil, err
		}
		return bound.Literal{V: v}, nil

	case qir.Column:
		return b.resolveColumn(x)

	case qir.Unary:
		sub, err := b.bindExpr(x.X)
		if err != nil {
			return nil, err
		}
		switch x.Op {
		case qir.OpNot:
			if sub.Type().Kind != qir.KindBool {
				return nil, fmt.Errorf("planner: not needs a boolean, got %s", sub.Type())
			}
		case qir.OpNegate:
			if !sub.Type().Kind.Numeric() {
				return nil, fmt.Errorf("planner: cannot negate %s", sub.Type())
			}
		case qir.OpIsNull, qir.OpIsNotNull:
			if !sub.Type().Kind.Scalar() {
				return nil, fmt.Errorf("planner: %s needs a scalar value, got %s", x.Op, sub.Type().Kind)
			}
		default:
			return nil, fmt.Errorf("planner: unknown unary operator %q", x.Op)
		}
		return bound.NewUnary(x.Op, sub), nil

	case qir.Binary:
		return b.bindBinary(x)

	case qir.Call:
		// The function registry is empty this arc.
		return nil, fmt.Errorf("planner: unknown function %q", x.Fn)

	case qir.Cast:
		sub, err := b.bindExpr(x.X)
		if err != nil {
			return nil, err
		}
		if !x.To.Scalar() {
			return nil, fmt.Errorf("planner: cannot cast to %s", x.To)
		}
		from := sub.Type().Kind
		ok := from == x.To ||
			(from == qir.KindInt64 && x.To == qir.KindFloat64) ||
			(from == qir.KindFloat64 && x.To == qir.KindInt64)
		if !ok {
			return nil, fmt.Errorf("planner: cannot cast %s to %s", from, x.To)
		}
		return bound.NewCast(sub, x.To), nil

	case qir.Exists:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		return bound.Exists{Rel: rel}, nil

	case qir.First:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		if !rel.Card().AtMostOne() && !bound.Ordered(rel) {
			return nil, fmt.Errorf("planner: first over an unordered multi-row relation would make results depend on the access path — add an order or make the relation at-most-one")
		}
		return bound.NewFirst(rel), nil

	case qir.Scalar:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		if !rel.Card().AtMostOne() {
			return nil, fmt.Errorf("planner: scalar asserts at most one row, but the relation may produce more — aggregate it, slice it, or pin a unique key")
		}
		return bound.NewScalar(rel)

	case qir.Array:
		rel, err := b.bindSubRel(x.Rel)
		if err != nil {
			return nil, err
		}
		return bound.NewArray(rel), nil

	case nil:
		return nil, fmt.Errorf("planner: missing expression")
	default:
		return nil, fmt.Errorf("planner: unknown expression node %T", e)
	}
}

// bindSubRel binds a crossing's relation: the current scope stack stays
// visible — that is how correlation binds — but scopes bound inside the
// sub-relation never leak out.
func (b *binder) bindSubRel(r qir.Relation) (bound.Relation, error) {
	if r == nil {
		return nil, fmt.Errorf("planner: crossing needs a relation")
	}
	mark := len(b.scopes)
	rel, err := b.bindRel(r)
	if err != nil {
		return nil, err
	}
	b.scopes = b.scopes[:mark]
	return rel, nil
}

func (b *binder) resolveColumn(c qir.Column) (bound.SlotRef, error) {
	if c.Scope == "" {
		return bound.SlotRef{}, fmt.Errorf("planner: column %q needs a scope qualifier", c.Name)
	}
	entry, ok := b.findScope(c.Scope, 0)
	if !ok {
		return bound.SlotRef{}, fmt.Errorf("planner: unknown scope %q", c.Scope)
	}
	f, ok := entry.rel.Output().Lookup(c.Name)
	if !ok {
		return bound.SlotRef{}, fmt.Errorf("planner: scope %q has no column %q", c.Scope, c.Name)
	}
	return bound.SlotRef{Slot: f.Slot, Name: c.Scope + "." + c.Name, T: f.Type}, nil
}

// bindBinary binds both operands, coercing a raw literal against the other
// side's type — a JSON number becomes int64 or float64 by the column it
// meets, never by guessing.
func (b *binder) bindBinary(x qir.Binary) (bound.Expr, error) {
	switch x.Op {
	case qir.OpAnd, qir.OpOr:
		l, err := b.bindExpr(x.L)
		if err != nil {
			return nil, err
		}
		r, err := b.bindExpr(x.R)
		if err != nil {
			return nil, err
		}
		for _, side := range []bound.Expr{l, r} {
			if side.Type().Kind != qir.KindBool {
				return nil, fmt.Errorf("planner: %s needs boolean operands, got %s", x.Op, side.Type())
			}
		}
		return bound.NewBinary(x.Op, l, r), nil

	case qir.OpEq, qir.OpNe, qir.OpLt, qir.OpLte, qir.OpGt, qir.OpGte:
		l, r, err := b.bindOperands(x.L, x.R)
		if err != nil {
			return nil, err
		}
		lk, rk := l.Type().Kind, r.Type().Kind
		if !lk.Scalar() || !rk.Scalar() {
			return nil, fmt.Errorf("planner: cannot compare %s with %s", lk, rk)
		}
		if lk != rk {
			// No numeric widening in comparisons this arc: an int64 column
			// never silently compares against a float literal.
			return nil, fmt.Errorf("planner: cannot compare %s with %s", lk, rk)
		}
		return bound.NewBinary(x.Op, l, r), nil

	case qir.OpAdd, qir.OpSub, qir.OpMul, qir.OpDiv:
		l, r, err := b.bindOperands(x.L, x.R)
		if err != nil {
			return nil, err
		}
		if !l.Type().Kind.Numeric() || !r.Type().Kind.Numeric() {
			return nil, fmt.Errorf("planner: %s needs numeric operands, got %s and %s", x.Op, l.Type(), r.Type())
		}
		return bound.NewBinary(x.Op, l, r), nil

	default:
		return nil, fmt.Errorf("planner: unknown binary operator %q", x.Op)
	}
}

// bindOperands binds a comparison or arithmetic pair. When exactly one side
// is a raw literal, the other side binds first and the literal coerces to
// its type; a NULL literal adopts it.
func (b *binder) bindOperands(l, r qir.Expr) (bound.Expr, bound.Expr, error) {
	llit, lIsLit := l.(qir.Literal)
	rlit, rIsLit := r.(qir.Literal)

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
func coerceLiteral(raw any, want qir.Type) (bound.Literal, error) {
	if !want.Kind.Scalar() {
		return bound.Literal{}, fmt.Errorf("planner: a literal cannot be a %s", want.Kind)
	}
	ct := want.Kind.CatalogType()
	if raw == nil {
		return bound.Literal{V: qir.Null(ct)}, nil
	}

	fail := func() (bound.Literal, error) {
		return bound.Literal{}, fmt.Errorf("planner: expected a %s value, got %T (%v)", ct, raw, raw)
	}
	switch v := raw.(type) {
	case string:
		if ct != catalog.TypeText {
			return fail()
		}
		return bound.Literal{V: qir.Text(v)}, nil
	case bool:
		if ct != catalog.TypeBool {
			return fail()
		}
		return bound.Literal{V: qir.Bool(v)}, nil
	case json.Number:
		switch ct {
		case catalog.TypeInt64:
			n, err := strconv.ParseInt(v.String(), 10, 64)
			if err != nil {
				return bound.Literal{}, fmt.Errorf("planner: expected an int64 value, got %q", v.String())
			}
			return bound.Literal{V: qir.Int64(n)}, nil
		case catalog.TypeFloat64:
			f, err := v.Float64()
			if err != nil {
				return bound.Literal{}, fmt.Errorf("planner: expected a float64 value, got %q", v.String())
			}
			return bound.Literal{V: qir.Float64(f)}, nil
		}
		return fail()
	case int:
		return coerceGoInt(int64(v), ct)
	case int64:
		return coerceGoInt(v, ct)
	case float64:
		if ct != catalog.TypeFloat64 {
			return fail()
		}
		return bound.Literal{V: qir.Float64(v)}, nil
	}
	return fail()
}

func coerceGoInt(v int64, ct catalog.Type) (bound.Literal, error) {
	switch ct {
	case catalog.TypeInt64:
		return bound.Literal{V: qir.Int64(v)}, nil
	case catalog.TypeFloat64:
		return bound.Literal{V: qir.Float64(float64(v))}, nil
	}
	return bound.Literal{}, fmt.Errorf("planner: expected a %s value, got integer %d", ct, v)
}

// inferLiteral types a literal with no column context — a literal compared
// against another literal, or used arithmetically. Integers stay int64;
// a bare NULL has no type to adopt and is rejected.
func inferLiteral(raw any) (qir.Value, error) {
	switch v := raw.(type) {
	case nil:
		return qir.Value{}, fmt.Errorf("planner: a bare NULL literal needs a typed context")
	case string:
		return qir.Text(v), nil
	case bool:
		return qir.Bool(v), nil
	case int:
		return qir.Int64(int64(v)), nil
	case int64:
		return qir.Int64(v), nil
	case float64:
		return qir.Float64(v), nil
	case json.Number:
		if n, err := strconv.ParseInt(v.String(), 10, 64); err == nil {
			return qir.Int64(n), nil
		}
		if f, err := v.Float64(); err == nil {
			return qir.Float64(f), nil
		}
		return qir.Value{}, fmt.Errorf("planner: malformed number %q", v.String())
	}
	return qir.Value{}, fmt.Errorf("planner: unsupported literal %T", raw)
}
