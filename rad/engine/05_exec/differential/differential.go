// Package differential runs one query several ways and requires the results to
// agree. It is the reusable core of the engine's strongest tests: the chosen
// physical plan, the same query forced to full scans, and the naive reference
// interpreter must all produce the same answer. Path independence proves the
// engine equivalent to itself under different physical choices; the interpreter
// pins what the answer actually is. The package holds no queries and no data of
// its own — a runner supplies both (from fixtures or the generator) and a
// Subject to run them against, so e2e, the planner corpus, and the generative
// suite can all compose it.
package differential

import (
	"context"
	"fmt"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
	refexec "github.com/Southclaws/rad/rad/engine/05_exec/refexec"
)

// ThreeWay runs q by the subject's chosen plan, by a forced full scan, and
// through the reference interpreter fed scan, and reports the first
// disagreement. It returns nil when all three agree — or when all three fail
// the same way, a legitimate runtime error (e.g. checked overflow) with nothing
// to compare. A non-nil error is a real defect: an error split (some ways fail,
// others do not), a query that does not bind (a generator producing illegal
// LIR), or a result divergence.
//
// ordered selects the comparison. A bag compares as a multiset, insensitive to
// order; a sequence compares element by element, catching an ordering bug a
// multiset would miss — pass it only when q imposes a total order.
func ThreeWay(ctx context.Context, s Subject, scan ScanFunc, q lir.Query, ordered bool) error {
	chosen, errC := s.Execute(ctx, q)
	forced, errF := s.ExecuteForced(ctx, q)
	oracle, errO := refexec.InterpretQuery(ctx, s.Catalog(), refexec.ScanFunc(scan), q)

	if (errC != nil) != (errO != nil) || (errC != nil) != (errF != nil) {
		return fmt.Errorf("error split: chosen=%v forced=%v oracle=%v\nquery:\n%#v", errC, errF, errO, q)
	}
	if errC != nil {
		if _, berr := binder.Bind(ctx, s.Catalog(), q); berr != nil {
			return fmt.Errorf("query does not bind: %w\nquery:\n%#v", berr, q)
		}
		return nil
	}

	if ordered {
		if a, b := seqJSON(chosen), seqJSON(oracle); a != b {
			return fmt.Errorf("chosen plan != interpreter (row sequence)\n chosen: %s\n oracle: %s\nquery:\n%#v", a, b, q)
		}
		if a, b := seqJSON(chosen), seqJSON(forced); a != b {
			return fmt.Errorf("chosen plan != forced full scan (row sequence)\n chosen: %s\n forced: %s\nquery:\n%#v", a, b, q)
		}
		return nil
	}
	cm := multiset(chosen)
	if om := multiset(oracle); !sameMultiset(cm, om) {
		return fmt.Errorf("chosen plan != interpreter\n chosen: %v\n oracle: %v\nquery:\n%#v", cm, om, q)
	}
	if fm := multiset(forced); !sameMultiset(cm, fm) {
		return fmt.Errorf("chosen plan != forced full scan\n chosen: %v\n forced: %v\nquery:\n%#v", cm, fm, q)
	}
	return nil
}
