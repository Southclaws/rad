package exec

// Folding a set of rows into one object of scalars. The planner has already
// validated every term against the table (column exists; sum/avg are
// numeric), so this code trusts its terms and only implements the arithmetic
// and the NULL/empty rules.

import (
	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// foldAggs reduces rows to one scalar per term, keyed by term.As. Typing and
// empty-input rules follow the design: count is int64 and never NULL (0 when
// empty); sum keeps the column's numeric type; avg is always float64; min/max
// keep the column's type; sum/avg/min/max over no non-NULL input yield NULL.
func foldAggs(tbl catalog.Table, rows []lir.Row, aggs []lir.AggTerm) (lir.Row, error) {
	out := make(lir.Row, len(aggs))
	for _, a := range aggs {
		col, _ := tbl.Column(a.Column) // present for all fns but count()

		switch a.Fn {
		case lir.AggCount:
			var n int64
			if a.Column == "" {
				n = int64(len(rows))
			} else {
				for _, r := range rows {
					if v, ok := r[a.Column]; ok && !v.Null {
						n++
					}
				}
			}
			out[a.As] = lir.Int64(n)

		case lir.AggSum:
			var (sumI int64; sumF float64; any bool)
			for _, r := range rows {
				v := r[a.Column]
				if v.Null {
					continue
				}
				any = true
				if col.Type == catalog.TypeInt64 {
					sumI += v.Int64
				} else {
					sumF += v.Float64
				}
			}
			switch {
			case !any:
				out[a.As] = lir.Null(col.Type)
			case col.Type == catalog.TypeInt64:
				out[a.As] = lir.Int64(sumI)
			default:
				out[a.As] = lir.Float64(sumF)
			}

		case lir.AggAvg:
			var (sum float64; n int64)
			for _, r := range rows {
				v := r[a.Column]
				if v.Null {
					continue
				}
				n++
				if col.Type == catalog.TypeInt64 {
					sum += float64(v.Int64)
				} else {
					sum += v.Float64
				}
			}
			if n == 0 {
				out[a.As] = lir.Null(catalog.TypeFloat64)
			} else {
				out[a.As] = lir.Float64(sum / float64(n))
			}

		case lir.AggMin, lir.AggMax:
			var best lir.Value
			var have bool
			for _, r := range rows {
				v := r[a.Column]
				if v.Null {
					continue
				}
				if !have {
					best, have = v, true
					continue
				}
				c, err := v.Compare(best)
				if err != nil {
					return nil, err
				}
				if (a.Fn == lir.AggMin && c < 0) || (a.Fn == lir.AggMax && c > 0) {
					best = v
				}
			}
			if !have {
				out[a.As] = lir.Null(col.Type)
			} else {
				out[a.As] = best
			}
		}
	}
	return out, nil
}
