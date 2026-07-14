# BUG: colliding join output column names are accepted at the query root and silently collapse

> **STATUS: FIXED** (2026-07-14). The binder now rejects duplicate output
> attribute names at every observable boundary — the query root (many/first/
> exactly_one) and the First/Array crossings — with `invalid` /
> `projection_collision`, the same rule binding bodies already enforced. Now a
> green regression guard.
> Fix: `rad/engine/04_planner/bind.go` (`requireUniqueOutput`, called from
> `bindQuery`) and `bind_expr.go` (First/Array crossings).

## What was sent

A self-join of `items` to itself (`inner`, `on lit(true)`), ordered, taken as
the query root with `cardinality: many` — no projection between the join and
the root. The join's output row type therefore has duplicate attribute names:
`id, val` from the left scope `l` and `id, val` again from the right scope `r`.

Seed: `items = [ (i1,10), (i2,20) ]`. The cross product is 4 pairs; ordered by
`(l.id, r.id)` it is `(i1,i1),(i1,i2),(i2,i1),(i2,i2)`.

## What the engine did

It accepted the query and returned rows in which the left side's columns are
gone. For the reduced repro the output objects each carry only `{id, val}`
(three keys total per row instead of four), holding the RIGHT side's values —
the left side's `id`/`val` are silently discarded. Observed with the original
3-column probe:

```
got:  [{"grp":1,"id":"i2","tag":200}, {"grp":1,"id":"i1","tag":100}]
```

i.e. for the pair `(l=i1, r=i2)` the object is `{id:i2, val:20}` — `l`'s data
is lost. `frameToObject` (iter.go:54) faithfully emits one `ObjectField` per
output field, producing an object datum with duplicate keys; canonical-JSON
serialization then collapses the duplicates (last-wins), so the answer is
silently wrong.

## What the spec says

- `Node` (lir.schema.yaml:188-194): every operator "produces a relation whose
  every output attribute is addressable by its consumer (relational closure)."
  Two attributes named `id` are not addressable, and the response's JSON object
  cannot represent both.
- `ProjectNode` (lir.schema.yaml:363-366): "Output attribute names must be
  unique. Every collision is rejected at bind time."
- The binder ALREADY enforces exactly this for a binding body, with a message
  aimed at precisely this shape: bind.go:130-136 rejects a raw join body with
  colliding column names ("binding %q output has duplicate column %q — project
  the body to a unique set of columns").

So colliding output attribute names are illegal by design; the query root
simply skips the check that binding bodies get.

## Root cause

`binder.bindBody` (rad/engine/04_planner/bind.go): the duplicate-column check
at lines 130-136 runs only over each binding body's `Output().Fields`. The
root is bound at line 142 (`b.bindRel(q.Root)`) with no equivalent uniqueness
check, and `bindQuery` (lines 60-73) validates column *count* (for `scalar`),
ordering, and cardinality but never output-name uniqueness. A join is the only
operator that can concatenate two row types into colliding names, so any join
at the root (or, by the same omission, materialised through a `first`/`array`
crossing) whose two sides share a column name renders a collapsed object.

## Fix direction

Apply the same duplicate-output-name check the binding path uses (bind.go:130)
to the query root — and to crossing outputs — rejecting with `invalid`. This
fixture asserts that rejection (`code: invalid`, detail contains
"duplicate column"); it is RED against the current engine, which returns a
(wrong) result instead of an error.
