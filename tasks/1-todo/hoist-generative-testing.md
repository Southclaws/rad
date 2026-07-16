# Hoist generative testing into a first-class toolkit

The generative engine tests are buried deep, but they are probably the most
important test tool in the codebase. Hoist them into `./tests` as a first-class
sibling of the other top-level test tools, and — the real prize — turn the
generator into a toolkit that can run against **arbitrary schemas defined in a
directory tree**, the way `tests/e2e` runs fixtures.

## What exists today

Everything lives in `package exec` as `_test.go` files under
`rad/engine/05_exec/`:

- `generate_test.go` — the whole generative differential: a synthetic random
  catalog (`genCatalogSpec`), random data, and a correct-by-construction typed
  LIR generator (`gen`), plus `TestGeneratedDifferential` (multiset),
  `TestGeneratedDifferentialOrdered` (row sequence), and `TestGeneratorCoverage`
  (a feature-distribution audit with floors and documented gaps).
- `conformance_test.go` — `executeFullScan` (force every access to a filtered
  table scan) and the fixed path-independence corpus.
- `oracle_test.go` — `interpQuery` (bind, then evaluate through the reference
  interpreter in `05_exec/refexec`) and the fixed oracle corpus.
- `execute_test.go` — `jsonish` (Datum → comparable `any`) and batched≡nested.
- `replay_test.go` — replay determinism over the same corpus.

The generator is genuinely good: typed and correct-by-construction (it tracks
each relation's output schema and only emits legal children — typed literals,
unique output names, an order where one is required, join sides that can't see
each other), it never emits arbitrary JSON, and it self-checks that a bind
failure is a *generator* bug. The three-way agreement (chosen plan ≡ forced
full scan ≡ reference interpreter) is what makes it load-bearing.

## Why it's buried (the private-dependency problem)

The generator can't move as-is because its three execution modes reach into
unexported `package exec` internals. Concretely:

- `interpQuery` uses `e.cat`, `e.store` (unexported fields) and `scanTable`
  (unexported), then calls the already-public `refexec.Interpret`.
- `executeFullScan` uses `e.cat`, `e.store`, `newExecutor`, `ex.commit`,
  `ex.build`, `bound.Env{}`, `drainOp`, `frameToObject`, `pp.Out` — it hand-
  drives the executor to realise a `FullScanOnly` plan.
- `jsonish` is a private render helper.

So the coupling is exactly two capabilities the public API doesn't have yet:
**execute-with-forced-full-scan** and **execute-through-the-reference-
interpreter**. Everything else the toolkit needs is *already public*:

- `frontend.Open(store)` → `DB`; `db.Execute(ctx, lir.Query)` (chosen plan);
  `db.Insert`; `db.MigrateFile(name, src)` / `db.Migrate(*schema.Schema)`;
  `db.Tables(ctx)` → `[]catalog.Table`; `db.Catalog()` → `*catalog.Catalog`;
  `frontend.DatumJSON` / `MarshalDatum`.
- `planner.Bind(ctx, cat, q)`, `planner.PlanQuery(q, planner.FullScanOnly())`
  are exported (`db.Catalog()` satisfies the `planner.Catalog` the binder wants,
  so the toolkit can run the diagnostic bind-check itself).
- `catalog.Table` fully describes a schema for introspection: `Columns`
  (`Name`/`Type`/`Nullable`/`Default`/`Format`), `PrimaryKey`, `Indexes`,
  `ForeignKeys`.

## The seam: two composable framework packages + one public engine method

The advanced test tooling is treated as real packages, not `_test.go` files,
because Go has no cross-package test-only export (`export_test.go` reaches only
the same directory) — and, more to the point, because the pieces are worth
reusing across every runner. Two small frameworks, composed by the runners:

- **`rad/engine/05_exec/generative`** — the query/data synthesiser: the spec
  model, synthetic catalog + data generation, the correct-by-construction query
  generator, the coverage feature-walk, and `Introspect` (a migrated
  `catalog.Table[]` → spec, restricted to the shape the generator drives). Pure
  `lir`/`catalog`; no `testing`, `exec`, or `frontend`. Sibling of `refexec`.
- **`rad/engine/05_exec/differential`** — runs one query several ways and
  requires agreement. `ThreeWay(ctx, Subject, scan, q, ordered) error` runs the
  chosen plan, the forced full scan, and the reference interpreter, enforcing
  the error-split/bind-check contract and comparing (multiset or sequence). It
  holds no queries or data — a runner supplies both plus a `Subject`, so e2e,
  the planner corpus, and the generative suite can all compose it. Imports
  `lir`/`catalog`/`planner`/`refexec`; never `exec` or `frontend`.

The three execution modes the differential needs are all reachable publicly:

- **chosen** = `Execute` — already public.
- **oracle** = `refexec.InterpretQuery(ctx, cat, scan, q)` (bind + interpret,
  scan injected) — `refexec` gains this so the interpreter and its glue stay one
  self-contained package (imports `planner`, never `exec`).
- **forced full scan** = a **new public method**, `exec.Engine.ExecuteForced`
  (+ `frontend.DB.ExecuteForced` delegating). This is the one capability that
  genuinely must drive the private executor, so it can't be a free function
  outside `exec`. Publishing it is justified: it's the backbone of shared
  verification tooling used across the whole suite, and doubles as a real
  "run forcing full scans" capability (EXPLAIN/verification), not a one-off test
  hook.

`differential.Subject` is `{ Execute; ExecuteForced; Catalog() }` — satisfied by
both `*frontend.DB` and the raw `*exec.Engine`, so a runner can point the same
differential at either level.

The oracle is fed the rows the runner **inserted**, not a storage re-read: it's
stronger, since the chosen plan reads through insert→encode→decode while the
oracle sees the pre-insert ground truth, so a storage round-trip bug surfaces as
a divergence instead of hiding in both sides.

With the seam in place, `exec`'s own path-independence test
(`conformance_test.go`) drops its hand-rolled `executeFullScan` for
`eng.ExecuteForced`, and the synthetic three-way leaves `package exec` entirely
(it no longer needs any private) to live in the composed runner.

## The toolkit shape

Three sibling framework packages under the executor, plus a thin consumer under
`./tests`:

```
rad/engine/05_exec/
  refexec/          // the reference interpreter (already existed)
  generative/       // spec, synthetic catalog+data, query generator, coverage,
                    //   Introspect, TableDefs, LIR constructors. pure lir/catalog.
  differential/     // Subject interface + ThreeWay + compare. no exec/frontend.
  execute.go        // + Engine.ExecuteForced

rad/engine/06_frontend/
  execute.go        // + DB.ExecuteForced (delegates)

tests/gen/          // the consumer: glue + Test bodies + fixtures
  generative_test.go   // discovers dirs; synthetic + schema-directed; composes
                       //   generative + differential against a frontend.DB
  <schema-name>/
    schema.rad         // required
    generative.json    // optional manifest (seeds, modes, bounds, floors) [phase 2]
    seed.json          // optional fixed seed data (else synthesised) [phase 2]
```

Discovery mirrors `tests/e2e/e2e_test.go`: each subdirectory with a `schema.rad`
is one scenario; the runner is fixture-agnostic and never grows per scenario.
`TestGeneratorCoverage` lives in `generative` (a `coverage_test.go` beside the
generator it audits), needing no engine at all.

### Two generation sources, one differential

- **Synthetic** (today's behaviour): random spec → random data → random queries.
  Keep it as the always-on fuzz mode (a `_synthetic` pseudo-scenario, or just a
  top-level subtest), env-tunable via `RAD_GEN_SEEDS`.
- **Schema-directed** (new): `MigrateFile(schema.rad)` into a fresh in-process
  `DB`, `Tables()` → introspect into the generator's spec, then synthesise data
  (or load `seed.json`) and generate queries typed to *that* schema. Same
  three-way differential and coverage audit.

### Phase 1 settles structure; generalisation is phase 2

**Do not generalise the generator in the first pass.** Phase 1 is purely
structural: the seam, the relocation, the directory runner, the manifest, and
schema-directed mode working end-to-end. To keep that honest without a
generator rewrite, phase 1 restricts schema-directed scenarios to the shape the
generator already drives (single text `id` PK per table, nullable text FKs,
non-unique indexes) and has `introspect.go` **skip-with-logged-reason** any
schema that uses a shape outside that subset. Authoring one or two compatible
`schema.rad` scenarios (plus the always-on synthetic mode) proves the structure;
the awkward-schema coverage comes later.

### Phase 2 — DONE: the generator fires at any arbitrary schema

Two things landed. First, the generator draws every choice from a
`pgregory.net/rapid` `*rapid.T` (`spec.go`/`data.go`/`query.go`), so a failing
case **minimises automatically** — the whole triple (schema, data, query)
shrinks; depth is tuned by `RAPID_CHECKS=N` / `-rapid.checks=N` (the full-suite
`task test` pins a light `RAPID_CHECKS=20`), and choices are ordered so rapid's
shrink-toward-first yields simpler queries.

Second, the synthetic-shape assumptions are gone; the spec (`Table` with
`PrimaryKey`/`Uniques`/`Indexes`/`FKs`) describes any Rad schema and `Introspect`
no longer skips anything:

- **Keys of any arity/type.** `genScope` carries the unique key; scans seed it
  from `Table.PrimaryKey`; `orderedSub` orders crossing bodies by it (not by a
  hardcoded `"id"`). Data-gen mints distinct primary-key values per type and
  dedupes candidate rows on the key.
- **Correlation/joins off real types.** `correlate`/`genJoinOn` already pick a
  shared *type* and fall back to `true` — so they generalise for free.
- **Uniqueness + foreign keys.** Data-gen respects secondary unique indexes
  (NULL-distinct) by dropping colliding candidates, and fills FK columns by
  copying a referenced parent row's values (multi-column and typed FKs, FK
  columns that are also part of the primary key, self- and empty-parent cases
  all handled — a non-satisfiable non-null FK simply drops the row).

Proven by two `tests/gen` fixtures: `library/` (the text-`id` subset) and
`composite/` (int64 PK, composite PK, a PK column that is also a foreign key to
a non-text key, a secondary unique index). Both run the full three-way
differential green.

Remaining polish (not blocking the goal): synthesise awkward shapes in
`SynthCatalog` too (so the fuzzer, not just fixtures, exercises them);
column defaults; the **metamorphic layer** (query→query relations, no oracle) —
now trivially composable on top of the generator + differential.

### Manifest (`generative.json`) — deferred

A per-scenario manifest is not built yet; phase 1/2 synthesise data and tune
depth via `RAPID_CHECKS`. When wanted:

Data, not code, like e2e. All optional with sensible defaults:

- `seeds` / `seedRange` — count or explicit range (overrides `RAD_GEN_SEEDS`).
- `modes` — subset of `["bag", "ordered"]`.
- `rows` — per-table row-count bounds for synthesised data.
- `useSeedFile` — use `seed.json` instead of synthesising data.
- `coverageFloors` / `expectGaps` — per-schema `mustHit`/gap lists so the audit
  stays honest against a fixed schema (a small schema legitimately can't reach
  every construct).

## Migration plan (each step green)

Phase 1 — structure — **DONE:**

1. `refexec.InterpretQuery(ctx, cat, scan, q)` (bind + `Interpret`), keeping the
   interpreter and its glue one self-contained package.
2. `exec.Engine.ExecuteForced` + `frontend.DB.ExecuteForced` (public); the one
   new capability. `conformance_test.go` drops its `executeFullScan` helper for
   it.
3. `rad/engine/05_exec/generative` — the pure generator (spec, synth, data,
   query, coverage, `Introspect`, `TableDefs`, constructors). `Multiset`/etc.
   moved into `differential`. `TestGeneratorCoverage` moved here.
4. `rad/engine/05_exec/differential` — `Subject`, `ThreeWay`, compare helpers.
5. `tests/gen` — the composed runner: synthetic + schema-directed, both bag and
   ordered, driving a `frontend.DB` through `differential.ThreeWay` fed the
   inserted dataset; one compatible `schema.rad` scenario (`library/`). The old
   `generate_test.go` in `package exec` is retired (its three-way needs no
   privates now).
6. `task test:generative` (→ `./tests/gen/`), `RAD_GEN_SEEDS` soak knob retained.

Phase 2 — capability expansion (separate, later): lift the generator
assumptions per the phase-2 section, adding awkward-schema scenarios (composite
/ non-text PK, unique index, typed FK) as each lift lands and its skip is
removed. The manifest (`generative.json` / `seed.json`) is phase-2 too — phase 1
synthesises data and defaults seeds via `RAD_GEN_SEEDS`.

## Open questions

- **Reusing the frameworks in `e2e`/`planner`.** `differential.ThreeWay` +
  `generative` are now standalone; adapting the other runners to compose them is
  the natural follow-on (the reason the seam was published).
- **Determinism across schemas.** Seed → (spec, data, query) must stay
  reproducible per scenario; the seed space is per-directory so a failing seed
  reproduces against its own `schema.rad`. The failure message already prints
  the reproducing seed and `%#v` query — keep that.
- **Cost.** Schema-directed runs multiply seeds × scenarios; default seed counts
  per scenario should stay modest (the synthetic soak mode is where big
  `RAD_GEN_SEEDS` counts go).

(Resolved: `Interpret` lives in the engine layer; generalisation is deferred to
phase 2 so the structure settles first.)
