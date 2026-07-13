package bound

import (
	"fmt"

	"github.com/Southclaws/rad/rad/engine/reject"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Env is an evaluation environment: one datum per slot in scope — the
// current row's own slots and any outer slots a correlated expression
// references. Scalar slots hold scalar datums (a NULL value is the null
// datum; SlotRef's static type restores it), row and array slots hold the
// materialised nested datums. One value vocabulary, one map.
type Env map[lir.SlotID]lir.Datum

// SetScalar stores a scalar value, collapsing NULL into the null datum.
func (e Env) SetScalar(slot lir.SlotID, v lir.Value) { e[slot] = lir.ScalarDatum(v) }

// ScalarAt reads a slot as a scalar value, restoring a typed NULL from t.
// Row and array datums are not scalars and error — expressions containing
// crossings were extracted before execution, so a non-scalar here is a
// planner bug, not a query.
func (e Env) ScalarAt(slot lir.SlotID, name string, t lir.Type) (lir.Value, error) {
	d, ok := e[slot]
	if !ok {
		return lir.Value{}, fmt.Errorf("exec: slot %d (%s) not in scope", slot, name)
	}
	switch d.Kind {
	case lir.DatumScalar:
		return d.Scalar, nil
	case lir.DatumNull:
		return lir.Null(t.Kind.CatalogType()), nil
	}
	return lir.Value{}, fmt.Errorf("exec: slot %d (%s) holds a nested value, not a scalar", slot, name)
}

// EvalPred evaluates a Bool-typed expression to a Kleene tri-bool. Filter
// keeps only TriTrue; NOT of UNKNOWN stays UNKNOWN — this is where
// `NOT (x = 1)` stops matching NULL rows.
//
// Expressions are pure: crossings never reach evaluation (the planner
// extracts every one into an attach slot), so there is no I/O, no context,
// and no relation evaluator behind an expression.
func EvalPred(e Expr, env Env) (lir.TriBool, error) {
	switch x := e.(type) {
	case Binary:
		switch x.Op {
		case lir.OpAnd:
			l, err := EvalPred(x.L, env)
			if err != nil {
				return lir.TriUnknown, err
			}
			if l == lir.TriFalse {
				return lir.TriFalse, nil
			}
			r, err := EvalPred(x.R, env)
			if err != nil {
				return lir.TriUnknown, err
			}
			return l.And(r), nil
		case lir.OpOr:
			l, err := EvalPred(x.L, env)
			if err != nil {
				return lir.TriUnknown, err
			}
			if l == lir.TriTrue {
				return lir.TriTrue, nil
			}
			r, err := EvalPred(x.R, env)
			if err != nil {
				return lir.TriUnknown, err
			}
			return l.Or(r), nil
		case lir.OpEq, lir.OpNe, lir.OpLt, lir.OpLte, lir.OpGt, lir.OpGte:
			return evalCompare(x, env)
		}
	case Unary:
		switch x.Op {
		case lir.OpNot:
			v, err := EvalPred(x.X, env)
			if err != nil {
				return lir.TriUnknown, err
			}
			return v.Not(), nil
		case lir.OpIsNull, lir.OpIsNotNull:
			// Works on any datum: a nested slot (an absent first) is NULL
			// exactly like a NULL scalar.
			d, err := EvalDatum(x.X, env)
			if err != nil {
				return lir.TriUnknown, err
			}
			if x.Op == lir.OpIsNull {
				return lir.FromBool(d.Kind == lir.DatumNull), nil
			}
			return lir.FromBool(d.Kind != lir.DatumNull), nil
		}
	}

	// Anything else Bool-typed — a bool column, literal, cast — evaluates as
	// a value; NULL is UNKNOWN.
	v, err := Eval(e, env)
	if err != nil {
		return lir.TriUnknown, err
	}
	if v.Null {
		return lir.TriUnknown, nil
	}
	if v.Type != catalog.TypeBool {
		return lir.TriUnknown, fmt.Errorf("exec: predicate evaluated to %s, want bool", v.Type)
	}
	return lir.FromBool(v.Bool), nil
}

func evalCompare(b Binary, env Env) (lir.TriBool, error) {
	l, err := Eval(b.L, env)
	if err != nil {
		return lir.TriUnknown, err
	}
	r, err := Eval(b.R, env)
	if err != nil {
		return lir.TriUnknown, err
	}
	if l.Null || r.Null {
		return lir.TriUnknown, nil
	}
	c, err := l.Compare(r)
	if err != nil {
		return lir.TriUnknown, fmt.Errorf("exec: %w", err)
	}
	switch b.Op {
	case lir.OpEq:
		return lir.FromBool(c == 0), nil
	case lir.OpNe:
		return lir.FromBool(c != 0), nil
	case lir.OpLt:
		return lir.FromBool(c < 0), nil
	case lir.OpLte:
		return lir.FromBool(c <= 0), nil
	case lir.OpGt:
		return lir.FromBool(c > 0), nil
	default: // gte
		return lir.FromBool(c >= 0), nil
	}
}

// Eval evaluates an expression to a scalar value. Bool-typed predicate forms
// yield Bool values with UNKNOWN as NULL. Crossings never appear here — the
// planner extracted them into slots — and nested-typed slots are values, not
// scalars; both are internal errors, not query errors.
func Eval(e Expr, env Env) (lir.Value, error) {
	switch x := e.(type) {
	case Literal:
		return x.V, nil

	case SlotRef:
		if !x.T.Kind.Scalar() {
			return lir.Value{}, fmt.Errorf("exec: slot %d (%s) holds a %s, not a scalar", x.Slot, x.Name, x.T.Kind)
		}
		return env.ScalarAt(x.Slot, x.Name, x.T)

	case Binary:
		switch x.Op {
		case lir.OpAdd, lir.OpSub, lir.OpMul, lir.OpDiv:
			return evalArith(x, env)
		default: // comparisons and and/or: predicate forms as values
			t, err := EvalPred(x, env)
			if err != nil {
				return lir.Value{}, err
			}
			return triValue(t), nil
		}

	case Unary:
		switch x.Op {
		case lir.OpNegate:
			v, err := Eval(x.X, env)
			if err != nil {
				return lir.Value{}, err
			}
			if v.Null {
				return lir.Null(v.Type), nil
			}
			switch v.Type {
			case catalog.TypeInt64:
				return lir.Int64(-v.Int64), nil
			case catalog.TypeFloat64:
				return lir.Float64(-v.Float64), nil
			}
			return lir.Value{}, fmt.Errorf("exec: cannot negate %s", v.Type)
		default: // not, is_null, is_not_null
			t, err := EvalPred(x, env)
			if err != nil {
				return lir.Value{}, err
			}
			return triValue(t), nil
		}

	case Cast:
		return evalCast(x, env)

	case Exists, First, Scalar, Array:
		return lir.Value{}, fmt.Errorf("exec: unextracted crossing %T reached evaluation", e)
	}
	return lir.Value{}, fmt.Errorf("exec: cannot evaluate %T", e)
}

// EvalDatum evaluates an expression to a datum: nested-typed slot references
// yield their materialised datum verbatim, everything else evaluates as a
// scalar and wraps. This is how a first or array output is referenced,
// reprojected, and spread like any other column.
func EvalDatum(e Expr, env Env) (lir.Datum, error) {
	if ref, ok := e.(SlotRef); ok && !ref.T.Kind.Scalar() {
		d, ok := env[ref.Slot]
		if !ok {
			return lir.Datum{}, fmt.Errorf("exec: slot %d (%s) not in scope", ref.Slot, ref.Name)
		}
		return d, nil
	}
	v, err := Eval(e, env)
	if err != nil {
		return lir.Datum{}, err
	}
	return lir.ScalarDatum(v), nil
}

// triValue renders a tri-bool as a Bool value with UNKNOWN as NULL.
func triValue(t lir.TriBool) lir.Value {
	switch t {
	case lir.TriTrue:
		return lir.Bool(true)
	case lir.TriFalse:
		return lir.Bool(false)
	default:
		return lir.Null(catalog.TypeBool)
	}
}

func evalArith(b Binary, env Env) (lir.Value, error) {
	l, err := Eval(b.L, env)
	if err != nil {
		return lir.Value{}, err
	}
	r, err := Eval(b.R, env)
	if err != nil {
		return lir.Value{}, err
	}
	resultType := b.t.Kind.CatalogType()
	if l.Null || r.Null {
		return lir.Null(resultType), nil
	}

	if resultType == catalog.TypeInt64 {
		if b.Op == lir.OpDiv && r.Int64 == 0 {
			return lir.Value{}, reject.Runtimef("exec: division by zero")
		}
		switch b.Op {
		case lir.OpAdd:
			return lir.Int64(l.Int64 + r.Int64), nil
		case lir.OpSub:
			return lir.Int64(l.Int64 - r.Int64), nil
		case lir.OpMul:
			return lir.Int64(l.Int64 * r.Int64), nil
		default:
			return lir.Int64(l.Int64 / r.Int64), nil
		}
	}

	lf, rf := asFloat(l), asFloat(r)
	if b.Op == lir.OpDiv && rf == 0 {
		return lir.Value{}, reject.Runtimef("exec: division by zero")
	}
	switch b.Op {
	case lir.OpAdd:
		return lir.Float64(lf + rf), nil
	case lir.OpSub:
		return lir.Float64(lf - rf), nil
	case lir.OpMul:
		return lir.Float64(lf * rf), nil
	default:
		return lir.Float64(lf / rf), nil
	}
}

func asFloat(v lir.Value) float64 {
	if v.Type == catalog.TypeInt64 {
		return float64(v.Int64)
	}
	return v.Float64
}

func evalCast(c Cast, env Env) (lir.Value, error) {
	v, err := Eval(c.X, env)
	if err != nil {
		return lir.Value{}, err
	}
	to := c.To.CatalogType()
	if v.Null {
		return lir.Null(to), nil
	}
	if v.Type == to {
		return v, nil
	}
	switch {
	case v.Type == catalog.TypeInt64 && to == catalog.TypeFloat64:
		return lir.Float64(float64(v.Int64)), nil
	case v.Type == catalog.TypeFloat64 && to == catalog.TypeInt64:
		return lir.Int64(int64(v.Float64)), nil
	}
	return lir.Value{}, fmt.Errorf("exec: cannot cast %s to %s", v.Type, to)
}
