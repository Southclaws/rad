# BUG: int64 arithmetic overflow silently wraps (two's complement)

> **STATUS: FIXED** (2026-07-14). int64 `add`/`sub`/`mul`/`div` and unary
> `negate` are now checked: an overflow (incl. `negate(MinInt64)` and
> `MinInt64 / -1`) returns `execution_failed`, consistent with the div-by-zero
> guard, instead of silently wrapping. Now a green regression guard.
> Fix: `rad/engine/03_lir/bound/eval.go` (`evalArith` int64 branch, `OpNegate`).

## What was sent

A one-row table with `a = 9223372036854775807` (MaxInt64) and `b = 1`, then a
query projecting `s = a add b` as a scalar.

```
project s = (t.a add t.b)   -- 9223372036854775807 + 1
```

## What the engine did

Returned **HTTP 200** with `s = -9223372036854775808` (MinInt64). The addition
overflowed and wrapped around under Go's silent two's-complement semantics.
No error, no warning — a mathematically wrong value delivered as a success.

## What should happen

The true value `9223372036854775808` is not representable as an int64. The
engine already treats the other undefined int64 operation — division by zero —
as a runtime failure (`reject.Runtimef("exec: division by zero")`, surfaced as
`execution_failed`; see `tests/e2e/errbind_division_by_zero`). Overflow is the
same class of undefined arithmetic and should likewise fail cleanly with
`execution_failed` rather than silently returning a wrong value. This fixture
asserts that correct behaviour, so it is RED against the current engine.

The `lir.schema.yaml` `BinaryExpr` prose (lines 638-640) says only that
arithmetic operators "operate on numeric operands and propagate NULL ... and
are evaluated." It does **not** document modular/wraparound semantics, so the
silent wrap is an undocumented silent-wrong — a correctness gap.

## Root cause

`rad/engine/03_lir/bound/eval.go`, `evalArith`, the int64 branch (lines
255-268):

```go
if resultType == catalog.TypeInt64 {
    if b.Op == lir.OpDiv && r.Int64 == 0 {
        return lir.Value{}, reject.Runtimef("exec: division by zero")
    }
    switch b.Op {
    case lir.OpAdd:
        return lir.Int64(l.Int64 + r.Int64), nil   // <- wraps on overflow
    case lir.OpSub:
        return lir.Int64(l.Int64 - r.Int64), nil   // <- wraps on overflow
    case lir.OpMul:
        return lir.Int64(l.Int64 * r.Int64), nil   // <- wraps on overflow
    ...
```

`add`/`sub`/`mul` use the raw Go `+`/`-`/`*` operators with no overflow check
(unlike the div-by-zero guard directly above them). Every overflowing int64
computation returns a wrapped value.

## Related / same family (not separate fixtures)

- `negate(MinInt64)` → `MinInt64` (overflow, wraps; `eval.go` OpNegate, line 188).
- `MinInt64 div -1` → `MinInt64` (Go defines this as a wrap, no panic; div guard
  only checks `r == 0`).
- `mul`/`sub` overflow wrap identically.
- `cast(float64 to int64)` of an out-of-range magnitude (e.g. `1e19`) is worse:
  Go's float→int conversion is *implementation-defined* out of range, so it is
  **platform-dependent** — arm64 saturates to MaxInt64 (`9223372036854775807`),
  amd64 wraps to MinInt64. Same root cause class in `evalCast`
  (`eval.go:309`, `lir.Int64(int64(v.Float64))`). Captured separately in
  `bug_cast_out_of_range_float`.

## No crash

Verified there is NO panic in any of these — the engine returns a value (or a
platform-specific value for cast), so the failure mode is silent-wrong, not a
dropped connection.
