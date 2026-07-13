package planner

import (
	"encoding/json"
	"github.com/Southclaws/rad/rad/engine/reject"
	"strconv"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

func (b *binder) bindExpr(e lir.Expr) (bound.Expr, error) {
	switch x := e.(type) {
	case lir.Literal:
		// A literal with no column context types itself from its raw form.
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
			return bound.SlotRef{}, reject.Inputf("planner: scope %q exists but is not visible here — a projection or aggregate closed it, or it belongs to a different sub-relation; label that node's output scope and reference its columns instead", c.Scope)
		}
		return bound.SlotRef{}, reject.Inputf("planner: unknown scope %q", c.Scope)
	}
	f, ok := entry.rel.Output().Lookup(c.Name)
	if !ok {
		return bound.SlotRef{}, reject.Inputf("planner: scope %q has no column %q", c.Scope, c.Name)
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
			// No numeric widening in comparisons this arc: an int64 column
			// never silently compares against a float literal.
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
		if ct != catalog.TypeText {
			return fail()
		}
		return bound.Literal{V: lir.Text(v)}, nil
	case bool:
		if ct != catalog.TypeBool {
			return fail()
		}
		return bound.Literal{V: lir.Bool(v)}, nil
	case json.Number:
		switch ct {
		case catalog.TypeInt64:
			n, err := strconv.ParseInt(v.String(), 10, 64)
			if err != nil {
				return bound.Literal{}, reject.Inputf("planner: expected an int64 value, got %q — cast the column to float64 to compare against fractional values", v.String())
			}
			return bound.Literal{V: lir.Int64(n)}, nil
		case catalog.TypeFloat64:
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
		if ct == catalog.TypeInt64 {
			return bound.Literal{}, reject.Inputf("planner: expected an int64 value, got %v — cast the column to float64 to compare against fractional values", v)
		}
		if ct != catalog.TypeFloat64 {
			return fail()
		}
		return bound.Literal{V: lir.Float64(v)}, nil
	}
	return fail()
}

func coerceGoInt(v int64, ct catalog.Type) (bound.Literal, error) {
	switch ct {
	case catalog.TypeInt64:
		return bound.Literal{V: lir.Int64(v)}, nil
	case catalog.TypeFloat64:
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
