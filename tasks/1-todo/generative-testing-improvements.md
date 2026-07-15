# Generative testing: remaining improvements

The core of the original plan is shipped (see `[[differential-property-oracle]]`
and `rad/engine/05_exec/generate_test.go` + `.../refexec`):

- typed LIR generation against a generated catalog, correct-by-construction
  from the leaves up, each node carrying its inferred output schema;
- typed expression generation by requested type (no `orders.total + name`);
- type-compatible joins incl. inner/left/self/inequality; correlation + crossings;
- a fuel/complexity budget; biased data (NULL, 0/±1, empty string, dups,
  nullable FKs); deterministic seeds;
- the three-way differential (engine chosen plan / forced full-scan / reference
  interpreter), multiset comparison, all-succeed-or-all-fail;
- a coverage audit (`TestGeneratorCoverage`) that tallies construct + composition
  frequency and floors it.

Already-handled comparison concerns (do not re-propose): objects compare by
sorted field name via canonical JSON, not map iteration order; NULL compares by
result equality (`null`), not predicate equality; duplicates are significant
(multiset counts), and Rad's LIR has no `distinct`, so the "unless distinct"
caveat is N/A.

What remains, roughly in value order:

## 1. Generate bindings / refs — DONE

The generator now emits 0–2 closed bindings (self-contained bodies, flattened to
a unique-named output so the single ref scope can't collide) and references each
at least once by joining a fresh `ref` into the tree — sometimes twice, which
drives the engine's materialise-vs-replay strategy split while the interpreter
commits once either way. `ref_binding` 0 → ~1375/2000 (in `mustHit`); the
coverage walker now descends binding bodies too. Differential holds over 1000
seeds — commit-once semantics agree with the interpreter, no engine bug. This is
the prerequisite for the binding-inlining metamorphic rule later.

Not yet: inter-binding references (a binding body referencing an earlier
binding) — bodies use plain `genRel`, no refs; a small follow-up.

## 2. Ordered-result comparison mode — DONE

`TestGeneratedDifferentialOrdered` generates queries with a *total* output
order (via `genOrderedQuery`) and sequence-compares the three paths exactly, so
ordering bugs (Sort, ordered-index pushdown) that the multiset mode can't see
now show up. The tie-order trap is handled by construction: the query projects
every scalar output column and orders by *all* of them, so the only ties are
between genuinely identical rows — which render the same in any order — making
the sequence well-defined regardless of the PK tie-breaker the engine appends
and the interpreter doesn't. (bool is included: `Value.Compare` totally orders
every scalar type; skipping it was what caused an empty-projection dead-end when
a mid-tree project dropped the id.) Holds over 1000 seeds; no engine
disagreement. The runner is shared with the multiset test via `runThreeWays`.

## 3. Compare errors by stable code/reason

The differential currently only checks all-paths-succeed vs all-paths-fail. Now
that errors carry a typed `{code, reason}` (`reject`/wire Problem), assert the
paths agree on the code/reason, not just on erroring, catches a case where
two paths fail for different reasons.

## 4. Enrich crossing sub-relations — DONE (crossing_over_join)

A correlated crossing's body can now be a filtered join, so `crossing_over_join`
went from 0 to ~600/2000 in the coverage audit (promoted into `mustHit`).
`exists`/`scalar` take the join body as-is (they render a bool / a count, never
the join's columns); `first`/`array` shape the body into objects, so those
flatten it to a unique-named output and order by the projected id columns (a
total unique key, so the selection is deterministic with no tie-break
divergence). That flatten is also what confirmed the binder correctly rejects a
"join body has duplicate id" crossing before the fix. Holds over 1000 seeds; no
engine disagreement (the flatten was a generator fix, not an engine bug).

Still a gap: `nested_crossing` (a crossing whose body contains another crossing)
— left for a later pass; a body sub-tree is a plain filtered scan/join, not yet
a projection carrying its own crossing fields.

## 5. Shrinking → automatic fixture emission

A failing seed today is reproducible but can be large. A greedy reducer (remove
rows, remove nodes, replace an expression with a bare column, replace a literal
with 0/1/NULL, drop one side of a boolean, remove a binding) that re-checks the
disagreement after each simplification, then emits the minimal case as a
permanent `tests/e2e` fixture + `BUG.md`. Turns the random suite into a
fixture-production machine so a discovered rule can't silently regress.

## 6. Distribution tuning (weighted choice / per-node cost)

Generation currently uses flat fuel + uniform operator choice. The coverage
audit shows some constructs are rarer than others; weighted selection and a
per-node cost budget (join/aggregate cost more) would flatten the distribution
toward the rarer-but-interesting compositions. The audit is the feedback loop;
tune against it.

## 7. Richer value domains — DONE (int64 extremes); found + fixed a sum bug

Generated data + literals now include `math.MinInt64` / `math.MaxInt64`. Chasing
this immediately exposed a latent bug the differential alone could *not* see
(the interpreter reimplemented the fold with the same silent wrap, so both
agreed while both wrong): int64 `sum` accumulated with raw `+=` and silently
wrapped on overflow. Fixed in both accumulators to compute the exact total via
`math/big` and error (`execution_failed`) iff the *true* total exceeds int64 —
**order-independent** (decided by the exact sum, not intermediate steps), so the
result never depends on aggregation order / access path (a property future
partial-aggregation optimisations must preserve). Pinned by an enumerated unit
test (`TestAggregateSumOverflow`, incl. order-independence) and e2e fixtures
`agg_sum_overflow` / `agg_sum_in_range` (which also prove int64 survives the
wire). Also hardened the differential harness: "all three paths errored"
consistently is now the all-fail *success* case (a legitimate runtime error like
overflow), distinguished from a generator-emitted un-bindable query by an
explicit bind check.

Remaining (lower value): large floats near the cast boundary and longer strings.
Floats need care — a float `sum` reaching ±Inf can't be JSON-compared, so keep
generated floats modest until an Inf-aware comparison exists.

## Deferred (bigger scope, own campaigns)

- Metamorphic transformations, `filter(Q,true) ≡ Q`, filter-merge,
  inner-join commutativity, binding inlining, and eventually
  `optimise(Q) ≡ Q`. Slots onto this harness with the interpreter refereeing
  rule correctness; deferred by choice until the sound-rule set is worth it (and
  #1 bindings is a prerequisite for the inlining rule).
- True in-memory oracle tables, the interpreter reads rows via an injected
  `ScanFunc`, today over the real store; feeding it a _separate_ in-memory table
  map (both populated from the same generated rows) would remove the shared
  row-codec from the trusted path. Independence nicety; low urgency (the codec
  is small and separately tested).
- PIR programs + transaction-state comparison, generate create/update/
  delete programs and differentially check resulting state against a simple
  in-memory transactional model. Read-only queries first; this is a large
  separate effort.

## Not applicable to Rad

- A `distinct` operator (LIR has none; bag semantics throughout).
- Generating structurally-invalid queries belongs to a _separate_ binder/
  validator fuzzer (negative space), not this positive-semantics differential —
  worth doing, different campaign.
