package lir

// The value vocabulary lives in 03_qir now — it is a storage-level primitive
// (rows persist as JSON of these values) shared by the surviving execution
// code and the new relation-graph IR. These aliases keep this package
// compiling while it is dismantled; they die with it.

import (
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

type Value = qir.Value

type Row = qir.Row

func Text(s string) Value       { return qir.Text(s) }
func Int64(i int64) Value       { return qir.Int64(i) }
func Float64(f float64) Value   { return qir.Float64(f) }
func Bool(b bool) Value         { return qir.Bool(b) }
func Null(t catalog.Type) Value { return qir.Null(t) }
