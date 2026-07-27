# BUG: correlated scalar/first/array/exists over a NULL key skipped the sub-plan

> **STATUS: FIXED** (2026-07-14). Found by the generated differential oracle
> (`TestGeneratedDifferential`) — the reference interpreter evaluates crossings
> per-row nested and returned `0`; the engine's batched attach returned `null`.
> Fix: `rad/engine/05_exec/attach.go` — a NULL correlation key is now an
> ordinary key group whose sub-plan still runs, instead of short-circuiting to
> a kind-based empty. Now a green regression guard.

## What was sent

For each `items` row, a correlated `scalar(count(...))` over the items whose
`id` equals this row's `ref`:

```
project i { id, n = scalar(count(items si where si.id == i.ref)) }
```

Seed: `i1` has `ref = NULL`; `i2` has `ref = "i1"`.

## What the engine did

Returned `n = null` for `i1` (the NULL-`ref` row), and `n = 1` for `i2`.

## What should happen

`i1`'s correlated filter matches nothing (`si.id == NULL` is UNKNOWN under 3VL),
but the crossing's sub-relation is a **global aggregate**, which produces one
row — `count = 0` — even over an empty input. So `scalar` must yield `0`, not
`null`. Correct result: `[{id:"i1", n:0}, {id:"i2", n:1}]`.

## Root cause

`rad/engine/05_exec/attach.go`, the `KeyCorrelated` batched path. A NULL key
short-circuited to `emptyAttach(a)` — a *kind-based* empty (`null` for
scalar/first, `[]` for array, `false` for exists) — "with no KV work at all".
That conflates "the correlated filter matches no base rows" with "the sub-plan
produces no rows". They differ whenever the sub-plan contains a global
aggregate (empty input ⇒ one row). The bug therefore also affected `first`,
`array`, and `exists` over a correlated global aggregate, not just `scalar`.

## Fix

Drop the short-circuit: a NULL key is just another distinct key group, and its
sub-plan runs like any other. The physical plan re-applies the full correlation
predicate as a residual filter above the keyed access, so a NULL key still
matches no base rows under 3VL — but the global aggregate above that empty
match correctly folds to its empty-input row. Verified across 400 generated
seeds with no NULL-key matches leaking.
