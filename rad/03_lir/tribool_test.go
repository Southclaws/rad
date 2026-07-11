package lir

import "testing"

// The full Kleene K3 truth tables. These are the normative semantics of
// every predicate in the IR; docs/design/lir-v2.md states them in prose.
func TestKleeneTables(t *testing.T) {
	F, U, T := TriFalse, TriUnknown, TriTrue

	and := [3][3]TriBool{
		//        F  U  T
		/* F */ {F, F, F},
		/* U */ {F, U, U},
		/* T */ {F, U, T},
	}
	or := [3][3]TriBool{
		/* F */ {F, U, T},
		/* U */ {U, U, T},
		/* T */ {T, T, T},
	}
	not := [3]TriBool{T, U, F}

	for a := F; a <= T; a++ {
		for b := F; b <= T; b++ {
			if got := a.And(b); got != and[a][b] {
				t.Errorf("%v AND %v = %v, want %v", a, b, got, and[a][b])
			}
			if got := a.Or(b); got != or[a][b] {
				t.Errorf("%v OR %v = %v, want %v", a, b, got, or[a][b])
			}
		}
		if got := a.Not(); got != not[a] {
			t.Errorf("NOT %v = %v, want %v", a, got, not[a])
		}
	}

	if FromBool(true) != T || FromBool(false) != F {
		t.Fatal("FromBool broken")
	}
}
