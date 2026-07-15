# ADR: shrinking + automatic fixture emission

Status: **todo** — spec for later; best done just before the optimiser effort,
where it pays off most (that's when rewrites start *producing* failures).

## What it is (the mental model)

Yes: shrinking is an **active, on-failure fixture-generation tool.** The random
differential (`TestGeneratedDifferential` / `…Ordered`) explores thousands of
generated cases; the moment one diverges (engine ≠ reference interpreter, or
chosen-plan ≠ full-scan), the shrinker kicks in, reduces that case to a minimal
form that *still* diverges, and writes it out as a permanent, human-readable
`tests/e2e` fixture (+ `BUG.md`). It does **not** generate tests continuously —
only when a failure is found. It turns the random suite from a bug *detector*
into a bug *detector + minimal-fixture producer*: the generated suite mines new
cases, and every real disagreement it finds becomes a small hand-fixture-shaped
regression that survives after the random seed is forgotten.

```
random seed diverges  →  shrink (delta-debug) to a minimal still-diverging case
                      →  emit tests/e2e/<name>/ {schema.rad, seed.json, test_*.json, BUG.md}
```

This is the persistence half of the loop we already do by hand (the `bug_*`
fixtures for the NULL-key and sum-overflow bugs were exactly this, done
manually). Shrinking automates it.

## Why it matters most for the optimiser

While the engine is green there is nothing to shrink. The value spikes when the
optimiser lands: an `optimise(Q) ≡ interpret(Q)` (or opt-on ≡ opt-off) check
over generated queries will find rewrites that change results, and those
failures are exactly the kind that are large and incomprehensible without
shrinking (a 30-node query over 4 tables). So: build it as the opening move of
the optimiser work, not before.

## Design: delta-debugging, not validity-preserving surgery

The clean design does **not** require reductions to be provably bind-valid.
Instead it is classic delta-debugging around an *interestingness predicate*:

```
P(case) = "the three paths still diverge for the SAME reason"
```

`P` rebuilds an engine from the case's catalog + rows, runs the query three ways
(reuse `runThreeWays`), and returns true iff there is a real divergence — **not**
all-agree, **not** a consistent runtime error, **not** a bind error. A reduction
that produces un-bindable LIR simply fails `P` and is discarded, so reductions
can be aggressive and best-effort; `P` is the filter. Greedily apply reductions,
keep any that preserve `P`, repeat until no reduction helps (fixpoint).

**Pin the divergence, not just "a" failure.** A reduction can accidentally
introduce a *different* bug. `P` should therefore match a *signature* of the
original divergence — at minimum "engine ≠ interpreter" vs "chosen ≠ full-scan"
(don't let one morph into the other); ideally also a coarse shape of the diff
(e.g. row-count mismatch vs value mismatch). Too-loose a predicate shrinks
toward the wrong bug; too-tight and it can't reduce. Start with the
mode-level signature and tighten if it misbehaves.

## What gets reduced (three targets)

A `case` is `{catalog spec, rows per table, query}` + the originating seed.

1. **Data** (cheapest, biggest wins first): drop a row; drop a whole table's
   rows; replace a value with a simpler one (`NULL` / `0` / `1` / `""`); collapse
   duplicate values.
2. **Query** (structural): replace a filter predicate with `true`; drop a
   projection field (not the last); drop a crossing field; replace an expression
   with a bare column or a `0`/`1`/`NULL` literal; drop one side of an `and`/`or`;
   replace a join with one of its inputs; replace any relation node with its
   input/child; remove a binding (and its refs) or inline it; reduce `slice`
   bounds. Each is a *candidate* — `P` decides if it holds.
3. **Catalog** (last): drop an unreferenced table; drop a column no surviving
   query node or FK needs. Ripples into data + query, so do it after those have
   shrunk and only when the result still binds.

## Cost

Each `P` evaluation rebuilds a store (~1s with `kvslate.Open`), and shrinking
tries many candidates — so naively it is slow. But shrinking runs only on a
found failure (rare), so seconds-to-a-minute is acceptable for an MVP. If it
bites: shrink the **query against a fixed engine+data first** (no store rebuild
per candidate — only the query changes), and only rebuild the store for the data
/ catalog reduction passes. An in-memory table map for the interpreter side
(the deferred storage-decoupling item) would also make the interpreter half of
`P` allocation-cheap.

## Emission

Once minimal, serialise the case to a fixture directory, same shape the hand
fixtures use so it reads identically:

- `schema.rad` from the (shrunk) catalog spec;
- `seed.json` from the (shrunk) rows, in FK-safe order;
- `test_<name>.json`: the query wrapped as a one-statement `query` program, with
  the **expected value taken from the reference interpreter** (the trusted
  oracle) — so the fixture is RED against the buggy engine and GREEN once fixed,
  exactly like the existing `bug_*` fixtures;
- `BUG.md`: the originating seed, the divergence (engine value vs interpreter
  value), and the reduction summary.

**Caveat to record in `BUG.md`:** the interpreter is trusted-by-construction but
not infallible; when engine ≠ interpreter the bug is *presumed* in the engine,
but a human confirms which side is wrong before pinning the expected value. The
emitter states both values so that review is one glance.

Naming: `bug_gen_<short-hash-or-slug>` so generated fixtures are distinguishable
from hand-authored ones but live in the same suite.

**No Go is generated** — the e2e runner is data-driven (it discovers fixture
directories; "a fixture is data, not code"), so emission is pure serialisation
and never triggers a compile. Formats: `schema.rad` is YAML, `seed.json` and
`test_*.json` are JSON.

**Serialisation wrinkle to plan for.** The generator builds the engine's
*nested* IR (`lir.Query` — `Filter{Input: …}`), while `test_*.json` stores the
*flat wire* form (`protocol` nodes-map JSON that `protocol.MarshalProgram` /
`MarshalQuery` emit and the client sends). Those are different representations,
and the existing conversion runs the *other* way (server: wire → engine IR). So
emission needs a `lir.Query → protocol` (nested → nodes-map) step, which likely
has to be written. Two clean options: (a) add that small converter and reuse
`MarshalProgram`; or (b) have the generator build the `protocol` wire model
directly and convert wire → `lir` (the existing server path) for the differential
run — which also removes the marshaller need at emit time. Decide when building;
neither is hard, but it is not free. `schema.rad` (from the catalog spec) and
`seed.json` (from the rows) are straightforward to serialise.

## Sequencing

1. `P` (interestingness predicate) with a mode-level divergence signature, built
   on `runThreeWays`.
2. The reducer loop over data reductions (fast, store-rebuild acceptable), then
   query reductions (query-only, fixed engine), to a fixpoint.
3. Emission to a `tests/e2e` fixture + `BUG.md`, expected = interpreter result.
4. Wire it into the differential tests: on a divergence, shrink + emit instead
   of a bare `t.Fatalf` (or behind a flag, so CI can either fail fast or capture).
5. (Optional) catalog reduction; query-against-fixed-data optimisation if slow.

## Non-goals

- Shrinking is not a correctness oracle — it only minimises a case the
  differential already flagged. Correctness still comes from the interpreter.
- Not a fuzzer of its own; it consumes the existing generator's failures.
- No property/metamorphic rules here (separate, deferred).
