# BUG: cast(float64 to int64) out of range is silently wrong AND platform-dependent

> **STATUS: FIXED** (2026-07-14). `evalCast` now range-checks the float before
> converting: NaN or a value outside `[MinInt64, MaxInt64]` returns
> `execution_failed` rather than an implementation-defined (platform-dependent)
> int64. Determinism restored. Now a green regression guard.
> Fix: `rad/engine/03_lir/bound/eval.go` (`evalCast` float64→int64 case).

## What was sent

A one-row table with `f = 1e19` (larger than MaxInt64 ≈ 9.22e18), then a query
projecting `n = cast(f to int64)` as a scalar.

## What the engine did

Returned **HTTP 200** with `n = 9223372036854775807` (MaxInt64) on this arm64
(darwin) host. The value is wrong — the true magnitude is 10000000000000000000
— and, worse, it is **platform-dependent**: Go's spec makes float→int
conversion of an out-of-range value *implementation-defined*. On amd64 the same
query returns `-9223372036854775808` (MinInt64). The same program therefore
yields different answers on different hardware, breaking determinism.

## What should happen

An out-of-range cast has no correct int64 result and should fail cleanly with
`execution_failed` (consistent with the div-by-zero runtime rejection), OR at
minimum produce a single documented, platform-independent value. This fixture
asserts `execution_failed`, so it is RED against the current engine.

The `lir.schema.yaml` `CastExpr` prose (lines 657-660) says the conversion is
"explicit" and nullable-preserving but says nothing about out-of-range
behaviour — so returning a platform-dependent garbage int64 is an undocumented
silent-wrong.

## Root cause

`rad/engine/03_lir/bound/eval.go`, `evalCast`, line 309:

```go
case v.Type == catalog.TypeFloat64 && to == catalog.TypeInt64:
    return lir.Int64(int64(v.Float64)), nil   // <- int64(float64) is UB out of range
```

`int64(v.Float64)` is a raw Go conversion with no range check; out-of-range
inputs hit Go's implementation-defined behaviour. There is no
`math.MinInt64 <= v <= math.MaxInt64` guard.

## No crash

Verified there is NO panic — the engine returns a (platform-specific) value, so
the failure mode is silent-wrong / non-deterministic, not a dropped connection.
