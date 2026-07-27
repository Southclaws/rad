# BUG: inverted-bound index range scan errors instead of returning empty

> **STATUS: FIXED** (2026-07-14). `scanIndexRange` now short-circuits to an
> empty iterator when the encoded `start >= end` (the KV rejects both inverted
> and zero-width ranges). This fixture is now a green regression guard.
> Fix: `rad/engine/05_exec/scanrange.go` (`emptyRowIter`, `start >= end` guard).

## What was sent

A query over table `t` (which has a secondary index on `n`) filtering
`n > 20 AND n < 20`. The binder accepts this — it is a legal conjunction of
two range predicates. The physical planner extracts the per-column domain
`n ∈ (20, 20)` and, because an index on `n` exists, chooses an
`IndexRangeScanExec` with `Lo = {20, exclusive}` and `Hi = {20, exclusive}`.

## What the engine did

    POST /execute 500
    internal error: Error: Invalid: Message=range start must not be greater than range end

A legal, bind-validated query yields an HTTP 500 internal error and no clean
RFC-7807 problem. `gt 30 AND lt 20` (disjoint bounds) fails identically.

## What should happen

The predicate is satisfiable by no row, so the correct result is the empty
list `[]`. An empty range must scan zero rows, not error.

## Root cause

`rad/engine/05_exec/scanrange.go`, `scanIndexRange` (lines 43-71).

`start` and `end` are computed independently from the low and high bounds:

- exclusive lo → `start = keyenc.PrefixEnd(prefix ++ enc(lo))`  (line 55)
- exclusive hi → `end   = prefix ++ enc(hi)`                    (line 66)

When `lo == hi` and both are exclusive (or when `lo > hi`), the encoded
`start` is strictly greater than `end`. There is no `start <= end` guard
before `view.Scan(ctx, start, end)` (line 71), so the inverted range is
passed straight to the KV, and kvslate/slateDB rejects it:
`range start must not be greater than range end`.

## Fix sketch

Before calling `view.Scan`, if `end != nil && bytes.Compare(start, end) >= 0`,
return an iterator that yields nothing (the range is provably empty). A
`start == end` range is already empty and happens to be accepted by the KV,
but `start > end` is not — a single guard covers both safely.
