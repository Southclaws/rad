# BUG: colliding join output column names collapse inside an `array` crossing

> **STATUS: FIXED** (2026-07-14). Closed by the same fix as
> `bug_join_dup_output_names`: `requireUniqueOutput` now runs on First/Array
> crossing outputs too. Now a green regression guard.
> Fix: `rad/engine/04_planner/bind_expr.go` (First/Array crossings).

Same root cause as `bug_join_dup_output_names`, exercising a DIFFERENT render
path: a crossing (`array`) materialised in a projection field rather than the
query root.

## What was sent

A projection field `pairs = array(node = self-join of items on lit(true),
ordered)`. The joined relation's output row type has duplicate names (`id, val`
from scope `l` and again from scope `r`).

Seed `items = [ (i1,10), (i2,20) ]`, so the cross product has 4 pairs.

## What the engine did

Accepted the query and returned:

```
{"k":1,"pairs":[{"id":"i1","val":10},{"id":"i2","val":20},{"id":"i1","val":10},{"id":"i2","val":20}]}
```

The array has the right element COUNT (4), but every element object collapsed
to a single `{id,val}` holding only the RIGHT side's values — the left side's
`id`/`val` are silently dropped. `frameToObject` (05_exec/attach.go via
iter.go:54) emits duplicate `ObjectField`s that collapse in canonical JSON.

## What the spec says / root cause

Identical to `bug_join_dup_output_names`: colliding output attribute names are
illegal (ProjectNode collision rule, lir.schema.yaml:363-366; relational
closure, :188-194) and the binder rejects them for binding bodies
(bind.go:130-136) — but the crossing-binding path (`bindSubRel` / `First` /
`Array` in 04_planner/bind_expr.go:78-106) never applies the uniqueness check,
just as the query root doesn't. A join is the only operator that can produce
colliding names.

The fix that closes `bug_join_dup_output_names` (apply the duplicate-name check
at every observable boundary) should also close this. This fixture asserts the
`invalid` rejection and is RED against the current engine.
