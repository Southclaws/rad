package eval

import (
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	. "github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

func lit(v lir.Value) Literal { return Literal{V: v} }

func scalarEnv(pairs map[lir.SlotID]lir.Value) Env {
	env := Env{}
	for k, v := range pairs {
		env.SetScalar(k, v)
	}
	return env
}

func pred(t *testing.T, e Expr, env Env) lir.TriBool {
	t.Helper()
	got, err := EvalPred(e, env)
	if err != nil {
		t.Fatal(err)
	}
	return got
}

// Every comparison with a NULL operand is UNKNOWN — and NOT(UNKNOWN) stays
// UNKNOWN, so NOT (x = 1) no longer matches NULL rows. This is the headline
// three-valued-logic fix.
func TestComparisonsWithNull(t *testing.T) {
	one := lit(lir.Int64(1))
	null := lit(lir.Null(model.TypeInt64))

	for _, op := range []lir.BinaryOp{lir.OpEq, lir.OpNe, lir.OpLt, lir.OpLte, lir.OpGt, lir.OpGte} {
		if got := pred(t, NewBinary(op, one, null), nil); got != lir.TriUnknown {
			t.Errorf("1 %s NULL = %v, want UNKNOWN", op, got)
		}
		if got := pred(t, NewBinary(op, null, null), nil); got != lir.TriUnknown {
			t.Errorf("NULL %s NULL = %v, want UNKNOWN", op, got)
		}
	}

	notEq := NewUnary(lir.OpNot, NewBinary(lir.OpEq, null, one))
	if got := pred(t, notEq, nil); got != lir.TriUnknown {
		t.Fatalf("NOT (NULL = 1) = %v, want UNKNOWN (a Filter drops it)", got)
	}

	// Non-null comparisons stay crisp.
	if pred(t, NewBinary(lir.OpEq, one, one), nil) != lir.TriTrue ||
		pred(t, NewBinary(lir.OpLt, one, lit(lir.Int64(2))), nil) != lir.TriTrue ||
		pred(t, NewBinary(lir.OpGt, one, lit(lir.Int64(2))), nil) != lir.TriFalse {
		t.Fatal("plain comparisons broken")
	}
}

// And/Or over expressions reproduce the K3 tables, including short-circuit
// soundness (F∧x=F, T∨x=T regardless of the other side).
func TestConnectivesK3(t *testing.T) {
	null := lit(lir.Null(model.TypeBool))
	tv := map[lir.TriBool]Expr{
		lir.TriTrue:    lit(lir.Bool(true)),
		lir.TriFalse:   lit(lir.Bool(false)),
		lir.TriUnknown: null,
	}
	for a, ea := range tv {
		for b, eb := range tv {
			if got := pred(t, NewBinary(lir.OpAnd, ea, eb), nil); got != a.And(b) {
				t.Errorf("%v AND %v = %v", a, b, got)
			}
			if got := pred(t, NewBinary(lir.OpOr, ea, eb), nil); got != a.Or(b) {
				t.Errorf("%v OR %v = %v", a, b, got)
			}
		}
		if got := pred(t, NewUnary(lir.OpNot, ea), nil); got != a.Not() {
			t.Errorf("NOT %v = %v", a, got)
		}
	}
}

// is_null / is_not_null are the only NULL-matching predicates, and are
// always two-valued.
func TestNullPredicates(t *testing.T) {
	null := lit(lir.Null(model.TypeText))
	val := lit(lir.Text("x"))

	if pred(t, NewUnary(lir.OpIsNull, null), nil) != lir.TriTrue ||
		pred(t, NewUnary(lir.OpIsNull, val), nil) != lir.TriFalse ||
		pred(t, NewUnary(lir.OpIsNotNull, null), nil) != lir.TriFalse ||
		pred(t, NewUnary(lir.OpIsNotNull, val), nil) != lir.TriTrue {
		t.Fatal("null predicates broken")
	}
}

// is_null works on nested slots too: an absent first is NULL exactly like a
// NULL scalar — one representation of absence.
func TestNullPredicatesOnNestedSlots(t *testing.T) {
	rowT := lir.Type{Kind: lir.KindRow, Nullable: true}
	ref := SlotRef{Slot: 3, Name: "owner", T: rowT}

	present := Env{3: lir.ObjectDatum([]lir.ObjectField{{Name: "id", Datum: lir.ScalarDatum(lir.Text("u1"))}})}
	absent := Env{3: lir.NullDatum()}

	if pred(t, NewUnary(lir.OpIsNull, ref), present) != lir.TriFalse ||
		pred(t, NewUnary(lir.OpIsNull, ref), absent) != lir.TriTrue ||
		pred(t, NewUnary(lir.OpIsNotNull, ref), present) != lir.TriTrue {
		t.Fatal("null predicates on nested slots broken")
	}
}

// A bare nullable bool attribute used as a predicate: NULL is UNKNOWN.
func TestBoolSlotAsPredicate(t *testing.T) {
	ref := SlotRef{Slot: 7, Name: "done", T: lir.Type{Kind: lir.KindBool, Nullable: true}}
	if pred(t, ref, scalarEnv(map[lir.SlotID]lir.Value{7: lir.Bool(true)})) != lir.TriTrue ||
		pred(t, ref, scalarEnv(map[lir.SlotID]lir.Value{7: lir.Bool(false)})) != lir.TriFalse ||
		pred(t, ref, scalarEnv(map[lir.SlotID]lir.Value{7: lir.Null(model.TypeBool)})) != lir.TriUnknown {
		t.Fatal("bool slot predicate broken")
	}
}

// A scalar slot round-trips through the datum env with its type restored —
// including a typed NULL, which the env stores as the null datum.
func TestScalarSlotRoundTrip(t *testing.T) {
	ref := SlotRef{Slot: 4, Name: "estimate", T: lir.Type{Kind: lir.KindFloat64, Nullable: true}}

	v, err := Eval(ref, scalarEnv(map[lir.SlotID]lir.Value{4: lir.Float64(2.5)}))
	if err != nil || v != lir.Float64(2.5) {
		t.Fatalf("scalar slot = %v, %v", v, err)
	}
	v, err = Eval(ref, scalarEnv(map[lir.SlotID]lir.Value{4: lir.Null(model.TypeFloat64)}))
	if err != nil || !v.Null || v.Type != model.TypeFloat64 {
		t.Fatalf("null slot = %v, %v (want typed NULL)", v, err)
	}
}

// Scalar operators reject non-scalar slots: a row datum is a value, not an
// operand.
func TestScalarOperatorsRejectNested(t *testing.T) {
	rowT := lir.Type{Kind: lir.KindRow, Nullable: true}
	ref := SlotRef{Slot: 3, Name: "owner", T: rowT}
	env := Env{3: lir.ObjectDatum(nil)}

	if _, err := Eval(ref, env); err == nil || !strings.Contains(err.Error(), "not a scalar") {
		t.Fatalf("nested slot as scalar = %v", err)
	}
}

// EvalDatum is the projection path: nested slots pass through verbatim,
// scalars wrap — so a first or array output reprojects like any column.
func TestEvalDatum(t *testing.T) {
	rowT := lir.Type{Kind: lir.KindRow, Nullable: true}
	obj := lir.ObjectDatum([]lir.ObjectField{{Name: "id", Datum: lir.ScalarDatum(lir.Text("u1"))}})
	env := Env{3: obj}
	env.SetScalar(4, lir.Int64(9))

	d, err := EvalDatum(SlotRef{Slot: 3, Name: "owner", T: rowT}, env)
	if err != nil || d.Kind != lir.DatumObject {
		t.Fatalf("nested slot datum = %v, %v", d, err)
	}
	d, err = EvalDatum(SlotRef{Slot: 4, Name: "n", T: lir.Type{Kind: lir.KindInt64}}, env)
	if err != nil || d.Kind != lir.DatumScalar || d.Scalar != lir.Int64(9) {
		t.Fatalf("scalar datum = %v, %v", d, err)
	}
	d, err = EvalDatum(NewBinary(lir.OpAdd, lit(lir.Int64(1)), lit(lir.Int64(2))), env)
	if err != nil || d.Scalar != lir.Int64(3) {
		t.Fatalf("computed datum = %v, %v", d, err)
	}
}

func TestArithmetic(t *testing.T) {
	got, err := Eval(NewBinary(lir.OpAdd, lit(lir.Int64(2)), lit(lir.Int64(3))), nil)
	if err != nil || got != lir.Int64(5) {
		t.Fatalf("2+3 = %v, %v", got, err)
	}
	// Integer division truncates.
	got, _ = Eval(NewBinary(lir.OpDiv, lit(lir.Int64(7)), lit(lir.Int64(2))), nil)
	if got != lir.Int64(3) {
		t.Fatalf("7/2 = %v", got)
	}
	// Mixed operands promote to float.
	got, _ = Eval(NewBinary(lir.OpMul, lit(lir.Int64(2)), lit(lir.Float64(1.5))), nil)
	if got != lir.Float64(3) {
		t.Fatalf("2*1.5 = %v", got)
	}
	// NULL propagates with the result's type.
	got, _ = Eval(NewBinary(lir.OpAdd, lit(lir.Int64(1)), lit(lir.Null(model.TypeInt64))), nil)
	if !got.Null || got.Type != model.TypeInt64 {
		t.Fatalf("1+NULL = %v", got)
	}
	// Division by zero is an error, not infinity.
	if _, err := Eval(NewBinary(lir.OpDiv, lit(lir.Int64(1)), lit(lir.Int64(0))), nil); err == nil {
		t.Fatal("int division by zero succeeded")
	}
	if _, err := Eval(NewBinary(lir.OpDiv, lit(lir.Float64(1)), lit(lir.Float64(0))), nil); err == nil {
		t.Fatal("float division by zero succeeded")
	}
	// Negation.
	got, _ = Eval(NewUnary(lir.OpNegate, lit(lir.Int64(4))), nil)
	if got != lir.Int64(-4) {
		t.Fatalf("-4 = %v", got)
	}
}

func TestCastEval(t *testing.T) {
	got, _ := Eval(NewCast(lit(lir.Int64(3)), lir.KindFloat64), nil)
	if got != lir.Float64(3) {
		t.Fatalf("cast int->float = %v", got)
	}
	got, _ = Eval(NewCast(lit(lir.Float64(3.9)), lir.KindInt64), nil)
	if got != lir.Int64(3) {
		t.Fatalf("cast float->int = %v (truncating)", got)
	}
	got, _ = Eval(NewCast(lit(lir.Null(model.TypeInt64)), lir.KindFloat64), nil)
	if !got.Null || got.Type != model.TypeFloat64 {
		t.Fatalf("cast NULL = %v", got)
	}
	if _, err := Eval(NewCast(lit(lir.Text("x")), lir.KindBool), nil); err == nil {
		t.Fatal("text->bool cast succeeded")
	}
}

// Expressions are pure: a crossing reaching evaluation is a planner bug —
// extraction happens before execution, always.
func TestCrossingsNeverEvaluate(t *testing.T) {
	s := NewRows("rows", nil, nil)

	if _, err := Eval(Exists{Rel: s}, nil); err == nil || !strings.Contains(err.Error(), "unextracted crossing") {
		t.Fatalf("exists eval = %v", err)
	}
	if _, err := Eval(NewFirst(s), nil); err == nil {
		t.Fatal("First evaluated as a scalar")
	}
	if _, err := Eval(NewArray(s), nil); err == nil {
		t.Fatal("Array evaluated as a scalar")
	}
}

func TestEvalErrors(t *testing.T) {
	if _, err := Eval(SlotRef{Slot: 9, Name: "ghost"}, Env{}); err == nil {
		t.Fatal("missing slot evaluated")
	}
}
