package lir

// Aggregation is a shape, not an operation. A relation — the root of a Read
// or a child Include — is either materialised as records or folded into one
// object of scalars. That choice is the Aggs field on Read/Include; there is
// deliberately no separate "Aggregate" node. Reads and includes are the one
// thing the IR knows how to do — materialise a relation — and Aggs only
// changes the cardinality of the result, the way Include.Dir already does.
//
// This is intentionally the minimal form. It leaves room for a later unified
// node with an explicit shape/cardinality (grouped rows, recursive fetches)
// without committing to that abstraction before three demo apps have pushed
// on it. Aggs is a shape annotation on a relation — it never becomes another
// arm of the Expr AST.

// AggFn names a fold.
type AggFn string

const (
	// AggCount counts rows (no column) or non-NULL values (with a column).
	// Its result is never NULL: an empty input counts to 0.
	AggCount AggFn = "count"
	// AggSum sums a numeric column, keeping its type: sum(int64) is int64,
	// sum(float64) is float64. NULL inputs are skipped; an empty input
	// sums to NULL.
	AggSum AggFn = "sum"
	// AggAvg averages a numeric column; the result is always float64.
	// NULLs are skipped; an empty input averages to NULL — never 0.
	AggAvg AggFn = "avg"
	// AggMin / AggMax take the extreme of any comparable column (text is
	// lexicographic; false < true). NULLs are skipped; empty input → NULL.
	AggMin AggFn = "min"
	AggMax AggFn = "max"
)

// AggTerm is one fold: Fn over Column, named As in the result object. Column
// is required for every function except count.
type AggTerm struct {
	Fn     AggFn
	Column string
	As     string
}
