package bound

import (
	"context"
	"fmt"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// Env is an evaluation environment: values for every slot in scope, both the
// current row's own slots and any outer slots a correlated expression
// references.
type Env map[qir.SlotID]qir.Value

// RelEvaluator executes the relation-consuming crossings that can appear in
// scalar position. The executor implements it; First and Array never reach
// scalar evaluation — projection compiles them to explicit materialisation
// operators.
type RelEvaluator interface {
	Exists(ctx context.Context, rel Relation, env Env) (bool, error)
	Scalar(ctx context.Context, rel Relation, env Env) (qir.Value, error)
}

// EvalPred evaluates a Bool-typed expression to a Kleene tri-bool. Filter
// keeps only TriTrue; NOT of UNKNOWN stays UNKNOWN — this is where
// `NOT (x = 1)` stops matching NULL rows.
func EvalPred(ctx context.Context, e Expr, env Env, re RelEvaluator) (qir.TriBool, error) {
	switch x := e.(type) {
	case Binary:
		switch x.Op {
		case qir.OpAnd:
			l, err := EvalPred(ctx, x.L, env, re)
			if err != nil {
				return qir.TriUnknown, err
			}
			if l == qir.TriFalse {
				return qir.TriFalse, nil
			}
			r, err := EvalPred(ctx, x.R, env, re)
			if err != nil {
				return qir.TriUnknown, err
			}
			return l.And(r), nil
		case qir.OpOr:
			l, err := EvalPred(ctx, x.L, env, re)
			if err != nil {
				return qir.TriUnknown, err
			}
			if l == qir.TriTrue {
				return qir.TriTrue, nil
			}
			r, err := EvalPred(ctx, x.R, env, re)
			if err != nil {
				return qir.TriUnknown, err
			}
			return l.Or(r), nil
		case qir.OpEq, qir.OpNe, qir.OpLt, qir.OpLte, qir.OpGt, qir.OpGte:
			return evalCompare(ctx, x, env, re)
		}
	case Unary:
		switch x.Op {
		case qir.OpNot:
			v, err := EvalPred(ctx, x.X, env, re)
			if err != nil {
				return qir.TriUnknown, err
			}
			return v.Not(), nil
		case qir.OpIsNull, qir.OpIsNotNull:
			v, err := Eval(ctx, x.X, env, re)
			if err != nil {
				return qir.TriUnknown, err
			}
			if x.Op == qir.OpIsNull {
				return qir.FromBool(v.Null), nil
			}
			return qir.FromBool(!v.Null), nil
		}
	case Exists:
		ok, err := re.Exists(ctx, x.Rel, env)
		if err != nil {
			return qir.TriUnknown, err
		}
		return qir.FromBool(ok), nil
	}

	// Anything else Bool-typed — a bool column, literal, cast, scalar
	// crossing — evaluates as a value; NULL is UNKNOWN.
	v, err := Eval(ctx, e, env, re)
	if err != nil {
		return qir.TriUnknown, err
	}
	if v.Null {
		return qir.TriUnknown, nil
	}
	if v.Type != catalog.TypeBool {
		return qir.TriUnknown, fmt.Errorf("exec: predicate evaluated to %s, want bool", v.Type)
	}
	return qir.FromBool(v.Bool), nil
}

func evalCompare(ctx context.Context, b Binary, env Env, re RelEvaluator) (qir.TriBool, error) {
	l, err := Eval(ctx, b.L, env, re)
	if err != nil {
		return qir.TriUnknown, err
	}
	r, err := Eval(ctx, b.R, env, re)
	if err != nil {
		return qir.TriUnknown, err
	}
	if l.Null || r.Null {
		return qir.TriUnknown, nil
	}
	c, err := l.Compare(r)
	if err != nil {
		return qir.TriUnknown, fmt.Errorf("exec: %w", err)
	}
	switch b.Op {
	case qir.OpEq:
		return qir.FromBool(c == 0), nil
	case qir.OpNe:
		return qir.FromBool(c != 0), nil
	case qir.OpLt:
		return qir.FromBool(c < 0), nil
	case qir.OpLte:
		return qir.FromBool(c <= 0), nil
	case qir.OpGt:
		return qir.FromBool(c > 0), nil
	default: // gte
		return qir.FromBool(c >= 0), nil
	}
}

// Eval evaluates an expression to a value. Bool-typed predicate forms yield
// Bool values with UNKNOWN as NULL. First and Array are not scalar values
// and are rejected here — projection materialises them explicitly.
func Eval(ctx context.Context, e Expr, env Env, re RelEvaluator) (qir.Value, error) {
	switch x := e.(type) {
	case Literal:
		return x.V, nil

	case SlotRef:
		v, ok := env[x.Slot]
		if !ok {
			return qir.Value{}, fmt.Errorf("exec: slot %d (%s) not in scope", x.Slot, x.Name)
		}
		return v, nil

	case Binary:
		switch x.Op {
		case qir.OpAdd, qir.OpSub, qir.OpMul, qir.OpDiv:
			return evalArith(ctx, x, env, re)
		default: // comparisons and and/or: predicate forms as values
			t, err := EvalPred(ctx, x, env, re)
			if err != nil {
				return qir.Value{}, err
			}
			return triValue(t), nil
		}

	case Unary:
		switch x.Op {
		case qir.OpNegate:
			v, err := Eval(ctx, x.X, env, re)
			if err != nil {
				return qir.Value{}, err
			}
			if v.Null {
				return qir.Null(v.Type), nil
			}
			switch v.Type {
			case catalog.TypeInt64:
				return qir.Int64(-v.Int64), nil
			case catalog.TypeFloat64:
				return qir.Float64(-v.Float64), nil
			}
			return qir.Value{}, fmt.Errorf("exec: cannot negate %s", v.Type)
		default: // not, is_null, is_not_null
			t, err := EvalPred(ctx, x, env, re)
			if err != nil {
				return qir.Value{}, err
			}
			return triValue(t), nil
		}

	case Cast:
		return evalCast(ctx, x, env, re)

	case Exists:
		t, err := EvalPred(ctx, x, env, re)
		if err != nil {
			return qir.Value{}, err
		}
		return triValue(t), nil

	case Scalar:
		return re.Scalar(ctx, x.Rel, env)

	case Call:
		return qir.Value{}, fmt.Errorf("exec: unknown function %q", x.Fn)

	case First, Array:
		return qir.Value{}, fmt.Errorf("exec: %T is not a scalar value; it materialises only in a projection field", e)
	}
	return qir.Value{}, fmt.Errorf("exec: cannot evaluate %T", e)
}

// triValue renders a tri-bool as a Bool value with UNKNOWN as NULL.
func triValue(t qir.TriBool) qir.Value {
	switch t {
	case qir.TriTrue:
		return qir.Bool(true)
	case qir.TriFalse:
		return qir.Bool(false)
	default:
		return qir.Null(catalog.TypeBool)
	}
}

func evalArith(ctx context.Context, b Binary, env Env, re RelEvaluator) (qir.Value, error) {
	l, err := Eval(ctx, b.L, env, re)
	if err != nil {
		return qir.Value{}, err
	}
	r, err := Eval(ctx, b.R, env, re)
	if err != nil {
		return qir.Value{}, err
	}
	resultType := b.t.Kind.CatalogType()
	if l.Null || r.Null {
		return qir.Null(resultType), nil
	}

	if resultType == catalog.TypeInt64 {
		if b.Op == qir.OpDiv && r.Int64 == 0 {
			return qir.Value{}, fmt.Errorf("exec: division by zero")
		}
		switch b.Op {
		case qir.OpAdd:
			return qir.Int64(l.Int64 + r.Int64), nil
		case qir.OpSub:
			return qir.Int64(l.Int64 - r.Int64), nil
		case qir.OpMul:
			return qir.Int64(l.Int64 * r.Int64), nil
		default:
			return qir.Int64(l.Int64 / r.Int64), nil
		}
	}

	lf, rf := asFloat(l), asFloat(r)
	if b.Op == qir.OpDiv && rf == 0 {
		return qir.Value{}, fmt.Errorf("exec: division by zero")
	}
	switch b.Op {
	case qir.OpAdd:
		return qir.Float64(lf + rf), nil
	case qir.OpSub:
		return qir.Float64(lf - rf), nil
	case qir.OpMul:
		return qir.Float64(lf * rf), nil
	default:
		return qir.Float64(lf / rf), nil
	}
}

func asFloat(v qir.Value) float64 {
	if v.Type == catalog.TypeInt64 {
		return float64(v.Int64)
	}
	return v.Float64
}

func evalCast(ctx context.Context, c Cast, env Env, re RelEvaluator) (qir.Value, error) {
	v, err := Eval(ctx, c.X, env, re)
	if err != nil {
		return qir.Value{}, err
	}
	to := c.To.CatalogType()
	if v.Null {
		return qir.Null(to), nil
	}
	if v.Type == to {
		return v, nil
	}
	switch {
	case v.Type == catalog.TypeInt64 && to == catalog.TypeFloat64:
		return qir.Float64(float64(v.Int64)), nil
	case v.Type == catalog.TypeFloat64 && to == catalog.TypeInt64:
		return qir.Int64(int64(v.Float64)), nil
	}
	return qir.Value{}, fmt.Errorf("exec: cannot cast %s to %s", v.Type, to)
}
