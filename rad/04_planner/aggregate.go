package planner

// Aggregate planning is a validation pass, not a new plan shape: a folded
// relation reuses the same access path as a record read (chooseAccess picks
// PK lookup / index scan / full scan from the filter). All this adds is
// checking that each fold makes sense against the column it names, so the
// executor can assume well-typed terms.

import (
	"fmt"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// validateAggs checks that every term names a real column of tbl (where a
// column is required), that sum/avg target a numeric column, and that the
// output names are present and distinct. It is used for both root aggregate
// reads and aggregate includes.
func validateAggs(tbl catalog.Table, aggs []lir.AggTerm) error {
	seen := map[string]bool{}
	for _, a := range aggs {
		if a.As == "" {
			return fmt.Errorf("planner: aggregate %s on table %q needs an output name (as)", a.Fn, tbl.Name)
		}
		if seen[a.As] {
			return fmt.Errorf("planner: duplicate aggregate output name %q", a.As)
		}
		seen[a.As] = true

		switch a.Fn {
		case lir.AggCount:
			// count() over rows needs no column; count(col) needs a real one.
			if a.Column != "" {
				if _, ok := tbl.Column(a.Column); !ok {
					return fmt.Errorf("planner: table %q has no column %q (count)", tbl.Name, a.Column)
				}
			}
		case lir.AggSum, lir.AggAvg:
			col, ok := tbl.Column(a.Column)
			if a.Column == "" || !ok {
				return fmt.Errorf("planner: %s needs a column of table %q, got %q", a.Fn, tbl.Name, a.Column)
			}
			if col.Type != catalog.TypeInt64 && col.Type != catalog.TypeFloat64 {
				return fmt.Errorf("planner: %s(%s) requires a numeric column, but %q is %s", a.Fn, a.Column, a.Column, col.Type)
			}
		case lir.AggMin, lir.AggMax:
			if a.Column == "" {
				return fmt.Errorf("planner: %s needs a column of table %q", a.Fn, tbl.Name)
			}
			if _, ok := tbl.Column(a.Column); !ok {
				return fmt.Errorf("planner: table %q has no column %q (%s)", tbl.Name, a.Column, a.Fn)
			}
		default:
			return fmt.Errorf("planner: unknown aggregate function %q", a.Fn)
		}
	}
	return nil
}
