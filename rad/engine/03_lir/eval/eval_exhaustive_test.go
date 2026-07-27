package eval

// Exhaustive, independently-pinned scalar semantics. These are the enumerated
// truth tables the differential oracle relies on: because the reference
// interpreter (rad/engine/05_exec/refexec) reuses this package's EvalPred /
// EvalDatum for scalar evaluation, a bug in scalar semantics would be invisible
// to the engine-vs-interpreter differential (both compute the same wrong
// answer). These tests close that gap by pinning the scalar rules against
// hand-authored / independently-computed expectations — never against the
// evaluator itself. They are what makes sharing the evaluator safe.
//
// The two bugs the recent adversarial sweep found (int64 overflow silently
// wrapping, float->int cast out of range being platform-dependent) lived in
// exactly this evaluator; the overflow and cast-range cases below are their
// regression pins at the unit level.

import (
	"math"
	"math/big"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// triExpr maps a tri-bool to a boolean expression carrying it: TRUE/FALSE as
// bool literals, UNKNOWN as a typed NULL bool.
func triExpr(t lir.TriBool) bound.Expr {
	switch t {
	case lir.TriTrue:
		return lit(lir.Bool(true))
	case lir.TriFalse:
		return lit(lir.Bool(false))
	default:
		return lit(lir.Null(model.TypeBool))
	}
}

// TestK3TruthTablesExplicit pins the Kleene tables as literal 3x3 (and 3x1)
// expectations, so the connectives are verified against the canonical tables
// directly, not against this package's own TriBool algebra.
func TestK3TruthTablesExplicit(t *testing.T) {
	T, F, U := lir.TriTrue, lir.TriFalse, lir.TriUnknown

	and := []struct{ a, b, want lir.TriBool }{
		{T, T, T},
		{T, F, F},
		{T, U, U},
		{F, T, F},
		{F, F, F},
		{F, U, F},
		{U, T, U},
		{U, F, F},
		{U, U, U},
	}
	for _, c := range and {
		if got := pred(t, bound.NewBinary(lir.OpAnd, triExpr(c.a), triExpr(c.b)), nil); got != c.want {
			t.Errorf("%v AND %v = %v, want %v", c.a, c.b, got, c.want)
		}
	}

	or := []struct{ a, b, want lir.TriBool }{
		{T, T, T},
		{T, F, T},
		{T, U, T},
		{F, T, T},
		{F, F, F},
		{F, U, U},
		{U, T, T},
		{U, F, U},
		{U, U, U},
	}
	for _, c := range or {
		if got := pred(t, bound.NewBinary(lir.OpOr, triExpr(c.a), triExpr(c.b)), nil); got != c.want {
			t.Errorf("%v OR %v = %v, want %v", c.a, c.b, got, c.want)
		}
	}

	not := []struct{ a, want lir.TriBool }{{T, F}, {F, T}, {U, U}}
	for _, c := range not {
		if got := pred(t, bound.NewUnary(lir.OpNot, triExpr(c.a)), nil); got != c.want {
			t.Errorf("NOT %v = %v, want %v", c.a, got, c.want)
		}
	}
}

// expectCmp is the definition of the comparison operators over an ordering
// result (cmp is -1/0/1 from an independent comparison). It is the oracle for
// TestComparisonMatrix — computed here, not by the evaluator.
func expectCmp(op lir.BinaryOp, cmp int) lir.TriBool {
	var b bool
	switch op {
	case lir.OpEq:
		b = cmp == 0
	case lir.OpNe:
		b = cmp != 0
	case lir.OpLt:
		b = cmp < 0
	case lir.OpLte:
		b = cmp <= 0
	case lir.OpGt:
		b = cmp > 0
	case lir.OpGte:
		b = cmp >= 0
	}
	if b {
		return lir.TriTrue
	}
	return lir.TriFalse
}

var orderedOps = []lir.BinaryOp{lir.OpEq, lir.OpNe, lir.OpLt, lir.OpLte, lir.OpGt, lir.OpGte}

// TestComparisonMatrix crosses every ordered comparison over int64, float64,
// and text against an independent ordering, and confirms any NULL operand is
// UNKNOWN.
func TestComparisonMatrix(t *testing.T) {
	ints := []int64{math.MinInt64, -1, 0, 1, math.MaxInt64}
	for _, a := range ints {
		for _, b := range ints {
			cmp := cmpInt(a, b)
			for _, op := range orderedOps {
				got := pred(t, bound.NewBinary(op, lit(lir.Int64(a)), lit(lir.Int64(b))), nil)
				if want := expectCmp(op, cmp); got != want {
					t.Errorf("int64 %d %s %d = %v, want %v", a, op, b, got, want)
				}
			}
		}
	}

	floats := []float64{-1.5, math.Copysign(0, -1), 0.0, 1.5, 2.5}
	for _, a := range floats {
		for _, b := range floats {
			cmp := cmpFloat(a, b)
			for _, op := range orderedOps {
				got := pred(t, bound.NewBinary(op, lit(lir.Float64(a)), lit(lir.Float64(b))), nil)
				if want := expectCmp(op, cmp); got != want {
					t.Errorf("float64 %v %s %v = %v, want %v", a, op, b, got, want)
				}
			}
		}
	}

	texts := []string{"", "a", "ab", "b"}
	for _, a := range texts {
		for _, b := range texts {
			cmp := cmpStr(a, b)
			for _, op := range orderedOps {
				got := pred(t, bound.NewBinary(op, lit(lir.Text(a)), lit(lir.Text(b))), nil)
				if want := expectCmp(op, cmp); got != want {
					t.Errorf("text %q %s %q = %v, want %v", a, op, b, got, want)
				}
			}
		}
	}

	// A NULL operand makes every comparison UNKNOWN, regardless of the other
	// side or the operator.
	nullI := lit(lir.Null(model.TypeInt64))
	for _, op := range orderedOps {
		if got := pred(t, bound.NewBinary(op, lit(lir.Int64(1)), nullI), nil); got != lir.TriUnknown {
			t.Errorf("1 %s NULL = %v, want UNKNOWN", op, got)
		}
		if got := pred(t, bound.NewBinary(op, nullI, lit(lir.Int64(1))), nil); got != lir.TriUnknown {
			t.Errorf("NULL %s 1 = %v, want UNKNOWN", op, got)
		}
	}
}

// TestIntArithmeticExhaustive pins int64 add/sub/mul/div/negate over a value
// set including the extremes, comparing against Go's big-int arithmetic as the
// independent oracle and requiring a clean execution_failed on overflow rather
// than a silent two's-complement wrap.
func TestIntArithmeticExhaustive(t *testing.T) {
	vals := []int64{math.MinInt64, math.MinInt64 + 1, -3, -2, -1, 0, 1, 2, 3, math.MaxInt64 - 1, math.MaxInt64}

	for _, a := range vals {
		// negate
		got, err := Eval(bound.NewUnary(lir.OpNegate, lit(lir.Int64(a))), nil)
		if a == math.MinInt64 {
			requireOverflow(t, err, "negate", a, 0)
		} else if err != nil || got != lir.Int64(-a) {
			t.Errorf("negate %d = %v, %v", a, got, err)
		}

		for _, b := range vals {
			checkInt(t, lir.OpAdd, a, b)
			checkInt(t, lir.OpSub, a, b)
			checkInt(t, lir.OpMul, a, b)
			checkInt(t, lir.OpDiv, a, b)
		}
	}
}

// checkInt evaluates `a op b` and compares to the exact big.Int result: an
// out-of-int64-range result (or division by zero) must be an execution_failed
// error; anything representable must match exactly.
func checkInt(t *testing.T, op lir.BinaryOp, a, b int64) {
	t.Helper()
	got, err := Eval(bound.NewBinary(op, lit(lir.Int64(a)), lit(lir.Int64(b))), nil)

	if op == lir.OpDiv && b == 0 {
		if err == nil {
			t.Errorf("%d / 0 succeeded = %v, want division-by-zero error", a, got)
		} else if !reject.IsRuntime(err) {
			t.Errorf("%d / 0 error = %v, want execution_failed", a, err)
		}
		return
	}

	want, representable := bigResult(op, a, b)
	if !representable {
		requireOverflow(t, err, string(op), a, b)
		return
	}
	if err != nil {
		t.Errorf("%d %s %d errored: %v (want %d)", a, op, b, err, want)
		return
	}
	if got != lir.Int64(want) {
		t.Errorf("%d %s %d = %v, want %d", a, op, b, got, want)
	}
}

func requireOverflow(t *testing.T, err error, op string, a, b int64) {
	t.Helper()
	if err == nil {
		t.Errorf("%s(%d,%d) did not overflow — silent wrap", op, a, b)
		return
	}
	if !reject.IsRuntime(err) {
		t.Errorf("%s(%d,%d) overflow error = %v, want execution_failed", op, a, b, err)
	}
}

// TestFloatArithmetic pins float64 arithmetic (no overflow trapping — IEEE),
// division by zero as an error, and NULL propagation.
func TestFloatArithmetic(t *testing.T) {
	cases := []struct {
		op       lir.BinaryOp
		a, b     float64
		want     float64
		divByArg bool
	}{
		{lir.OpAdd, 1.5, 2.25, 3.75, false},
		{lir.OpSub, 1.5, 2.25, -0.75, false},
		{lir.OpMul, 1.5, 2.0, 3.0, false},
		{lir.OpDiv, 3.0, 2.0, 1.5, false},
		{lir.OpDiv, 1.0, 0.0, 0, true},
	}
	for _, c := range cases {
		got, err := Eval(bound.NewBinary(c.op, lit(lir.Float64(c.a)), lit(lir.Float64(c.b))), nil)
		if c.divByArg {
			if err == nil || !reject.IsRuntime(err) {
				t.Errorf("%v / 0.0 = %v, %v, want execution_failed", c.a, got, err)
			}
			continue
		}
		if err != nil || got != lir.Float64(c.want) {
			t.Errorf("%v %s %v = %v, %v, want %v", c.a, c.op, c.b, got, err, c.want)
		}
	}

	// Mixed int/float promotes to float64.
	if got, _ := Eval(bound.NewBinary(lir.OpMul, lit(lir.Int64(2)), lit(lir.Float64(1.5))), nil); got != lir.Float64(3) {
		t.Errorf("2 * 1.5 = %v, want 3.0 (float)", got)
	}
}

// TestNullPropagationArithmetic: any NULL operand yields a typed NULL result,
// for every arithmetic op and both operand positions, int and float.
func TestNullPropagationArithmetic(t *testing.T) {
	for _, op := range []lir.BinaryOp{lir.OpAdd, lir.OpSub, lir.OpMul, lir.OpDiv} {
		gi, _ := Eval(bound.NewBinary(op, lit(lir.Int64(1)), lit(lir.Null(model.TypeInt64))), nil)
		if !gi.Null || gi.Type != model.TypeInt64 {
			t.Errorf("1 %s NULL(int) = %v, want typed NULL int64", op, gi)
		}
		gi, _ = Eval(bound.NewBinary(op, lit(lir.Null(model.TypeInt64)), lit(lir.Int64(1))), nil)
		if !gi.Null {
			t.Errorf("NULL(int) %s 1 = %v, want NULL", op, gi)
		}
		gf, _ := Eval(bound.NewBinary(op, lit(lir.Float64(1)), lit(lir.Null(model.TypeFloat64))), nil)
		if !gf.Null || gf.Type != model.TypeFloat64 {
			t.Errorf("1.0 %s NULL(float) = %v, want typed NULL float64", op, gf)
		}
	}
	// negate(NULL) is NULL.
	if g, _ := Eval(bound.NewUnary(lir.OpNegate, lit(lir.Null(model.TypeInt64))), nil); !g.Null {
		t.Errorf("negate NULL = %v, want NULL", g)
	}
}

// TestCastFloatToIntExhaustive pins float->int truncation toward zero and the
// out-of-range rule: NaN, ±Inf, and any magnitude ≥ 2^63 must be a clean
// execution_failed, never a platform-dependent conversion.
func TestCastFloatToIntExhaustive(t *testing.T) {
	trunc := []struct {
		f    float64
		want int64
	}{
		{0.0, 0},
		{0.9, 0},
		{-0.9, 0},
		{3.9, 3},
		{-3.9, -3},
		{3.0, 3},
		{-3.0, -3},
		{1.0, 1},
		{-1.0, -1},
		{9.0e18, 9000000000000000000}, // in range (< 2^63 ≈ 9.223e18)
	}
	for _, c := range trunc {
		got, err := Eval(bound.NewCast(lit(lir.Float64(c.f)), lir.KindInt64), nil)
		if err != nil || got != lir.Int64(c.want) {
			t.Errorf("cast %v to int64 = %v, %v, want %d", c.f, got, err, c.want)
		}
	}

	outOfRange := []float64{
		1e19, -1e19,
		math.MaxFloat64, -math.MaxFloat64,
		math.Inf(1), math.Inf(-1),
		math.NaN(),
		float64(math.MaxInt64), // 2^63, exclusive upper bound — not representable
	}
	for _, f := range outOfRange {
		got, err := Eval(bound.NewCast(lit(lir.Float64(f)), lir.KindInt64), nil)
		if err == nil {
			t.Errorf("cast %v to int64 = %v, want out-of-range error", f, got)
		} else if !reject.IsRuntime(err) {
			t.Errorf("cast %v to int64 error = %v, want execution_failed", f, err)
		}
	}

	// MinInt64 is exactly representable as float64 and must round-trip.
	if got, err := Eval(bound.NewCast(lit(lir.Float64(float64(math.MinInt64))), lir.KindInt64), nil); err != nil || got != lir.Int64(math.MinInt64) {
		t.Errorf("cast MinInt64.0 to int64 = %v, %v, want MinInt64", got, err)
	}
}

// bigResult computes a op b exactly and reports whether it fits in int64.
func bigResult(op lir.BinaryOp, a, b int64) (int64, bool) {
	x, y := big.NewInt(a), big.NewInt(b)
	z := new(big.Int)
	switch op {
	case lir.OpAdd:
		z.Add(x, y)
	case lir.OpSub:
		z.Sub(x, y)
	case lir.OpMul:
		z.Mul(x, y)
	case lir.OpDiv:
		z.Quo(x, y) // truncate toward zero, matching Go int division
	}
	if !z.IsInt64() {
		return 0, false
	}
	return z.Int64(), true
}

func cmpInt(a, b int64) int {
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	default:
		return 0
	}
}

func cmpFloat(a, b float64) int {
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	default:
		return 0
	}
}

func cmpStr(a, b string) int {
	switch {
	case a < b:
		return -1
	case a > b:
		return 1
	default:
		return 0
	}
}
