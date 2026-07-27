package eval

// Branch selection semantics, pinned enumeratively: arm predicates evaluate
// in document order under K3, only TriTrue matches, the first match's result
// is produced, and nothing outside the selected path is ever evaluated. The
// reference interpreter shares this evaluator, so these tests are what make
// that sharing safe.

import (
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// boom is an expression that errors if evaluated: 1 / 0. Placed in an arm
// that is never selected, it proves laziness — any division-by-zero error
// means the branch evaluated something it must not.
func boom() bound.Expr {
	return bound.NewBinary(lir.OpDiv, lit(lir.Int64(1)), lit(lir.Int64(0)))
}

// boomPred is a predicate that errors if evaluated: (1 / 0) = 1.
func boomPred() bound.Expr {
	return bound.NewBinary(lir.OpEq, boom(), lit(lir.Int64(1)))
}

func arm(when, then bound.Expr) bound.BranchArm {
	return bound.BranchArm{When: when, Then: then}
}

// The first TriTrue arm wins, in document order, even when later arms would
// also match.
func TestBranchFirstTrueArmWins(t *testing.T) {
	b := bound.NewBranch([]bound.BranchArm{
		arm(lit(lir.Bool(false)), lit(lir.Int64(1))),
		arm(lit(lir.Bool(true)), lit(lir.Int64(2))),
		arm(lit(lir.Bool(true)), lit(lir.Int64(3))),
	}, lit(lir.Int64(9)))

	got, err := Eval(b, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got.Null || got.Int64 != 2 {
		t.Fatalf("branch = %v, want 2", got)
	}
}

// FALSE and UNKNOWN predicates both fall through — matching is strictly
// TriTrue, never Go-boolean truthiness of a possibly-NULL bool.
func TestBranchK3FallThrough(t *testing.T) {
	unknown := lit(lir.Null(model.TypeBool))

	b := bound.NewBranch([]bound.BranchArm{
		arm(lit(lir.Bool(false)), lit(lir.Text("f"))),
		arm(unknown, lit(lir.Text("u"))),
	}, lit(lir.Text("else")))

	got, err := Eval(b, nil)
	if err != nil {
		t.Fatal(err)
	}
	if got.Null || got.Text != "else" {
		t.Fatalf("branch = %v, want else", got)
	}
}

// Unselected result expressions are never evaluated: the else after a TRUE
// arm, the then of a FALSE or UNKNOWN arm, and every predicate after the
// first match. Laziness is contract, not optimization.
func TestBranchLaziness(t *testing.T) {
	cases := map[string]bound.Branch{
		"else not evaluated after a true arm": bound.NewBranch([]bound.BranchArm{
			arm(lit(lir.Bool(true)), lit(lir.Int64(1))),
		}, boom()),
		"unmatched arm results not evaluated": bound.NewBranch([]bound.BranchArm{
			arm(lit(lir.Bool(false)), boom()),
			arm(lit(lir.Null(model.TypeBool)), boom()),
			arm(lit(lir.Bool(true)), lit(lir.Int64(1))),
		}, lit(lir.Int64(9))),
		"predicates after the match not evaluated": bound.NewBranch([]bound.BranchArm{
			arm(lit(lir.Bool(true)), lit(lir.Int64(1))),
			arm(boomPred(), lit(lir.Int64(2))),
		}, lit(lir.Int64(9))),
	}
	for name, b := range cases {
		got, err := Eval(b, nil)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		if got.Null || got.Int64 != 1 {
			t.Fatalf("%s: branch = %v, want 1", name, got)
		}
	}
}

// When no arm matches, the else is the result — including a typed NULL.
func TestBranchElseAndNullResult(t *testing.T) {
	b := bound.NewBranch([]bound.BranchArm{
		arm(lit(lir.Bool(false)), lit(lir.Int64(1))),
	}, lit(lir.Null(model.TypeInt64)))

	got, err := Eval(b, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !got.Null || got.Type != model.TypeInt64 {
		t.Fatalf("branch = %v, want typed NULL int64", got)
	}
}

// A bool-typed branch used as a predicate: its value maps to K3 like any
// other bool expression, with NULL as UNKNOWN.
func TestBranchAsPredicate(t *testing.T) {
	mk := func(when bound.Expr, then, els lir.Value) bound.Branch {
		return bound.NewBranch([]bound.BranchArm{arm(when, lit(then))}, lit(els))
	}
	if pred(t, mk(lit(lir.Bool(true)), lir.Bool(true), lir.Bool(false)), nil) != lir.TriTrue {
		t.Fatal("selected true arm should be TriTrue")
	}
	if pred(t, mk(lit(lir.Bool(false)), lir.Bool(true), lir.Bool(false)), nil) != lir.TriFalse {
		t.Fatal("else false should be TriFalse")
	}
	if pred(t, mk(lit(lir.Bool(false)), lir.Bool(true), lir.Null(model.TypeBool)), nil) != lir.TriUnknown {
		t.Fatal("NULL branch value should be UNKNOWN")
	}
}

// An error inside the selected path surfaces — laziness never suppresses the
// arm that actually runs.
func TestBranchSelectedArmErrorSurfaces(t *testing.T) {
	b := bound.NewBranch([]bound.BranchArm{
		arm(lit(lir.Bool(true)), boom()),
	}, lit(lir.Int64(9)))
	if _, err := Eval(b, nil); err == nil {
		t.Fatal("division by zero in the selected arm must error")
	}

	b = bound.NewBranch([]bound.BranchArm{
		arm(boomPred(), lit(lir.Int64(1))),
	}, lit(lir.Int64(9)))
	if _, err := Eval(b, nil); err == nil {
		t.Fatal("division by zero in an evaluated predicate must error")
	}
}
