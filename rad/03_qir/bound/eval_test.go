package bound

import (
	"context"
	"strings"
	"testing"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// fakeRel is a canned RelEvaluator for crossing tests.
type fakeRel struct {
	exists bool
	scalar qir.Value
}

func (f fakeRel) Exists(context.Context, Relation, Env) (bool, error)      { return f.exists, nil }
func (f fakeRel) Scalar(context.Context, Relation, Env) (qir.Value, error) { return f.scalar, nil }

func lit(v qir.Value) Literal { return Literal{V: v} }

func pred(t *testing.T, e Expr, env Env) qir.TriBool {
	t.Helper()
	got, err := EvalPred(context.Background(), e, env, fakeRel{})
	if err != nil {
		t.Fatal(err)
	}
	return got
}

// Every comparison with a NULL operand is UNKNOWN — and NOT(UNKNOWN) stays
// UNKNOWN, so NOT (x = 1) no longer matches NULL rows. This is the headline
// three-valued-logic fix.
func TestComparisonsWithNull(t *testing.T) {
	one := lit(qir.Int64(1))
	null := lit(qir.Null(catalog.TypeInt64))

	for _, op := range []qir.BinaryOp{qir.OpEq, qir.OpNe, qir.OpLt, qir.OpLte, qir.OpGt, qir.OpGte} {
		if got := pred(t, NewBinary(op, one, null), nil); got != qir.TriUnknown {
			t.Errorf("1 %s NULL = %v, want UNKNOWN", op, got)
		}
		if got := pred(t, NewBinary(op, null, null), nil); got != qir.TriUnknown {
			t.Errorf("NULL %s NULL = %v, want UNKNOWN", op, got)
		}
	}

	notEq := NewUnary(qir.OpNot, NewBinary(qir.OpEq, null, one))
	if got := pred(t, notEq, nil); got != qir.TriUnknown {
		t.Fatalf("NOT (NULL = 1) = %v, want UNKNOWN (a Filter drops it)", got)
	}

	// Non-null comparisons stay crisp.
	if pred(t, NewBinary(qir.OpEq, one, one), nil) != qir.TriTrue ||
		pred(t, NewBinary(qir.OpLt, one, lit(qir.Int64(2))), nil) != qir.TriTrue ||
		pred(t, NewBinary(qir.OpGt, one, lit(qir.Int64(2))), nil) != qir.TriFalse {
		t.Fatal("plain comparisons broken")
	}
}

// And/Or over expressions reproduce the K3 tables, including short-circuit
// soundness (F∧x=F, T∨x=T regardless of the other side).
func TestConnectivesK3(t *testing.T) {
	null := lit(qir.Null(catalog.TypeBool))
	tv := map[qir.TriBool]Expr{
		qir.TriTrue:    lit(qir.Bool(true)),
		qir.TriFalse:   lit(qir.Bool(false)),
		qir.TriUnknown: null,
	}
	for a, ea := range tv {
		for b, eb := range tv {
			if got := pred(t, NewBinary(qir.OpAnd, ea, eb), nil); got != a.And(b) {
				t.Errorf("%v AND %v = %v", a, b, got)
			}
			if got := pred(t, NewBinary(qir.OpOr, ea, eb), nil); got != a.Or(b) {
				t.Errorf("%v OR %v = %v", a, b, got)
			}
		}
		if got := pred(t, NewUnary(qir.OpNot, ea), nil); got != a.Not() {
			t.Errorf("NOT %v = %v", a, got)
		}
	}
}

// is_null / is_not_null are the only NULL-matching predicates, and are
// always two-valued.
func TestNullPredicates(t *testing.T) {
	null := lit(qir.Null(catalog.TypeText))
	val := lit(qir.Text("x"))

	if pred(t, NewUnary(qir.OpIsNull, null), nil) != qir.TriTrue ||
		pred(t, NewUnary(qir.OpIsNull, val), nil) != qir.TriFalse ||
		pred(t, NewUnary(qir.OpIsNotNull, null), nil) != qir.TriFalse ||
		pred(t, NewUnary(qir.OpIsNotNull, val), nil) != qir.TriTrue {
		t.Fatal("null predicates broken")
	}
}

// A bare nullable bool attribute used as a predicate: NULL is UNKNOWN.
func TestBoolSlotAsPredicate(t *testing.T) {
	ref := SlotRef{Slot: 7, Name: "done", T: qir.Type{Kind: qir.KindBool, Nullable: true}}
	if pred(t, ref, Env{7: qir.Bool(true)}) != qir.TriTrue ||
		pred(t, ref, Env{7: qir.Bool(false)}) != qir.TriFalse ||
		pred(t, ref, Env{7: qir.Null(catalog.TypeBool)}) != qir.TriUnknown {
		t.Fatal("bool slot predicate broken")
	}
}

func TestArithmetic(t *testing.T) {
	ctx := context.Background()
	re := fakeRel{}

	got, err := Eval(ctx, NewBinary(qir.OpAdd, lit(qir.Int64(2)), lit(qir.Int64(3))), nil, re)
	if err != nil || got != qir.Int64(5) {
		t.Fatalf("2+3 = %v, %v", got, err)
	}
	// Integer division truncates.
	got, _ = Eval(ctx, NewBinary(qir.OpDiv, lit(qir.Int64(7)), lit(qir.Int64(2))), nil, re)
	if got != qir.Int64(3) {
		t.Fatalf("7/2 = %v", got)
	}
	// Mixed operands promote to float.
	got, _ = Eval(ctx, NewBinary(qir.OpMul, lit(qir.Int64(2)), lit(qir.Float64(1.5))), nil, re)
	if got != qir.Float64(3) {
		t.Fatalf("2*1.5 = %v", got)
	}
	// NULL propagates with the result's type.
	got, _ = Eval(ctx, NewBinary(qir.OpAdd, lit(qir.Int64(1)), lit(qir.Null(catalog.TypeInt64))), nil, re)
	if !got.Null || got.Type != catalog.TypeInt64 {
		t.Fatalf("1+NULL = %v", got)
	}
	// Division by zero is an error, not infinity.
	if _, err := Eval(ctx, NewBinary(qir.OpDiv, lit(qir.Int64(1)), lit(qir.Int64(0))), nil, re); err == nil {
		t.Fatal("int division by zero succeeded")
	}
	if _, err := Eval(ctx, NewBinary(qir.OpDiv, lit(qir.Float64(1)), lit(qir.Float64(0))), nil, re); err == nil {
		t.Fatal("float division by zero succeeded")
	}
	// Negation.
	got, _ = Eval(ctx, NewUnary(qir.OpNegate, lit(qir.Int64(4))), nil, re)
	if got != qir.Int64(-4) {
		t.Fatalf("-4 = %v", got)
	}
}

func TestCastEval(t *testing.T) {
	ctx := context.Background()
	re := fakeRel{}

	got, _ := Eval(ctx, NewCast(lit(qir.Int64(3)), qir.KindFloat64), nil, re)
	if got != qir.Float64(3) {
		t.Fatalf("cast int->float = %v", got)
	}
	got, _ = Eval(ctx, NewCast(lit(qir.Float64(3.9)), qir.KindInt64), nil, re)
	if got != qir.Int64(3) {
		t.Fatalf("cast float->int = %v (truncating)", got)
	}
	got, _ = Eval(ctx, NewCast(lit(qir.Null(catalog.TypeInt64)), qir.KindFloat64), nil, re)
	if !got.Null || got.Type != catalog.TypeFloat64 {
		t.Fatalf("cast NULL = %v", got)
	}
	if _, err := Eval(ctx, NewCast(lit(qir.Text("x")), qir.KindBool), nil, re); err == nil {
		t.Fatal("text->bool cast succeeded")
	}
}

func TestCrossingsInScalarPosition(t *testing.T) {
	ctx := context.Background()
	s := scanTasks()

	// Exists delegates to the executor and is never NULL.
	if got := pred(t, Exists{Rel: s}, nil); got != qir.TriFalse {
		t.Fatalf("exists(fake false) = %v", got)
	}
	gotTrue, err := EvalPred(ctx, Exists{Rel: s}, nil, fakeRel{exists: true})
	if err != nil || gotTrue != qir.TriTrue {
		t.Fatalf("exists(fake true) = %v, %v", gotTrue, err)
	}

	// Scalar yields the executor's value.
	agg := NewAggregate(s, nil, []AggTerm{{Fn: qir.AggCount, Name: "n", Slot: 20, T: AggTermType(qir.AggCount, nil)}})
	sc, err := NewScalar(agg)
	if err != nil {
		t.Fatal(err)
	}
	v, err := Eval(ctx, sc, nil, fakeRel{scalar: qir.Int64(7)})
	if err != nil || v != qir.Int64(7) {
		t.Fatalf("scalar = %v, %v", v, err)
	}

	// First and Array are projection-only.
	if _, err := Eval(ctx, NewFirst(s), nil, fakeRel{}); err == nil {
		t.Fatal("First evaluated as a scalar")
	}
	if _, err := Eval(ctx, NewArray(s), nil, fakeRel{}); err == nil {
		t.Fatal("Array evaluated as a scalar")
	}
}

func TestEvalErrors(t *testing.T) {
	ctx := context.Background()
	re := fakeRel{}

	if _, err := Eval(ctx, Call{Fn: "lower"}, nil, re); err == nil || !strings.Contains(err.Error(), "unknown function") {
		t.Fatalf("call err = %v", err)
	}
	if _, err := Eval(ctx, SlotRef{Slot: 9, Name: "ghost"}, Env{}, re); err == nil {
		t.Fatal("missing slot evaluated")
	}
}
