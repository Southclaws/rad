# ADR: Oracle-based testing for the query engine

Status: **todo** — designed, ready to build. Read-only (queries) first.

## Decisions locked

- **Not** compiling LIR → SQL to differential-test against SQLite/Postgres.
  That would drag in dialect compatibility, alias/quoting, semantics inherited
  from another database, and pressure to support only the SQL-expressible
  subset — and it cuts against Rad's whole point (the IR *is* the interface).
  It stays a distant optional experiment, not roadmap.
- The near-term oracle is a **naïve in-memory LIR interpreter** run alongside
  the real engine on the same generated query, plus **metamorphic** rewrites.
- **Interpreter location:** `rad/engine/05_exec/refexec` — nested under exec
  (the numbered layers are strict and locked for layer clarity), but sharing no
  query logic with its parent: it takes table rows through an injected scan
  function and reimplements every relational operator itself (see "Where it
  lives").
- **Many catalog shapes**, generated — not one fixed schema.
- **CI budget: unconstrained** — no CI yet; generate aggressively, log the seed.
- **No `rad verify` CLI** yet.
- **Read-only:** generate queries, not PIR mutation programs, for v1.

## Problem

Testing a query engine has exactly one hard part: the **oracle** — the thing
that decides the correct answer for a query. Our ~147 `tests/e2e` fixtures are
golden-result oracles: a human writes the query *and* the answer. Invaluable as
spec examples and regression tests, but they only cover shapes a human thought
of, and the expected value is human-authored (confirm-bias risk). Oracles are
the next level up: they explore the space of semantics nobody remembered to
write down. The recent adversarial sweep found four real bugs exactly that way,
one-shot; we want that power systematically.

## The four oracle classes

1. **Golden-result** — our fixtures. `schema + rows + program → expected
   relation/problem`. Keep expanding them; they are the readable spec and the
   regression net. Weakness: human-authored answers.
2. **Differential (independent engine)** — same logical query through two
   independent executors; compare. **Out of scope** here (no SQL backend). The
   *independent executor we will use* is our own reference interpreter (class 3)
   — same idea, no second database.
3. **Reference-interpreter** — a brutally simple in-memory LIR evaluator that
   implements Rad semantics directly (not another DB's). The primary near-term
   oracle. It says what the answer *is*.
4. **Property / metamorphic** — no exact answer needed; transform a query into
   one that must be equivalent, or assert an invariant on the output. A second,
   independent axis of confidence that needs no oracle at all.

Rad's structured IR makes classes 3 and 4 *far* easier than for SQL databases:
we generate, type-check, rename, clone, and rewrite a query **graph** directly —
no string generation, parsing, alias-preservation tricks, or "are these two SQL
statements really the same rewrite" ambiguity.

## The near-term architecture

```text
generated typed LIR  (bind-valid, schema-aware)
        │  bind once → *bound.Query
        ├── naïve in-memory reference interpreter ─── relation A
        └── real planner → physical plan → KV executor ─ relation B
                                   compare A == B
```

Bind **once** and hand the same `*bound.Query` to both sides, so a binder
difference can't masquerade as an executor difference — this oracle isolates
**planner + physical plan + KV execution**, which is where the recent bugs
lived. **The split is deliberately after binding.** Sharing the binder is fine:
scope resolution, slot numbering, identifier validation, and type inference are
deterministic AST transforms, not the interesting semantics — reimplementing
them just for tests buys nothing. The interesting semantics begin once you are
executing relations, which is exactly where the two paths diverge. Binder
correctness is a separate concern with its own coverage (bound-IR golden tests,
the validation matrix, and the enumerated 3VL test below).

**Storage independence via the scan seam.** Because rows enter the interpreter
through an injected `ScanFunc` (below), the interpreter depends on *no* storage
implementation. The end-state harness feeds the same generated rows two ways —
a pure in-memory table map to the oracle, the real SlateDB/KV store to Rad —
so the oracle is a storage-free executable specification, not "Rad reading its
own bytes a second time":

```text
generated data
   ├──► oracle tables (in-memory map)      ──► reference interpreter ─ A
   └──► real Rad store (SlateDB/KV)         ──► planner → executor    ─ B
                                    A == B
```

## Piece 1 — the reference interpreter

Promote today's `interp`/`interpQuery` (in `rad/engine/05_exec/oracle_test.go`)
out of the test file into a real package. It already does the right thing:
full-scan every table, nested-loop joins, per-row crossing evaluation, an
independent `fold` for aggregation, its own order/slice, materialise everything.

Design law: **if a line of the interpreter is clever, it is wrong.** It may be
O(n²) and load everything into memory. It exists to be so obvious it needs no
tests of its own. Explicitly:

```text
scan       materialise all table rows
rows       materialise literal rows
filter     loop, evaluate predicate, keep TRUE
project    build new rows; crossings evaluated inline per row
join       nested loops; left join pads NULL on no match
aggregate  map keyed by encoded group values; naïve folds
order      in-memory stable sort by explicit Value.Compare
slice      offset/limit array slicing
ref        recursively evaluate/materialise the committed binding
```

**Independence rules** (the whole value is that it can't share a bug with the
executor):

- No reuse of planner predicates, physical operators, index scans, attach, or
  batching. Its own relational implementation, end to end.
- Shares only the **protocol/bound IR types** (the contract under test, like
  the wire types) and the primitive **`Value`** representation + its encoding.
- **Expression evaluation:** reusing `bound.EvalPred`/`EvalDatum` (3VL,
  arithmetic incl. the overflow/cast-range rules, comparisons) is the one
  defensible sharing, *because 3VL and scalar semantics are independently
  pinned by the enumerated truth-table test (Piece 4) and the `expr_*`/`f3vl_*`
  fixtures.* Start by reusing it; a fully independent evaluator (own 3VL) is a
  clean stretch that closes even that gap. Flag the choice in code.

### Where it lives

`rad/engine/05_exec/refexec` — nested under exec because the numbered layers are
strict and locked. The nesting is organisational only; the **independence rule
holds regardless**: refexec must share *no query logic* with `05_exec`. It
imports the IR (`03_lir`, `03_lir/bound`), `02_catalog`, and `01_kv/keyenc`
(low-level encoding contract) — and gets stored rows through an **injected scan
function**, so it never touches exec's scan/planner/operator/attach/index code.
Group keys use naïve linear bucketing (no shared tuple encoder); scalar
expression eval reuses `bound.Eval*` per Piece 1's argument.

Surface — rows are injected, not read via exec:

```go
// ScanFunc yields all stored rows of a table (materialised) from the caller's
// snapshot. The caller (the harness / a test with a kv.KV view) supplies it;
// refexec never imports exec's row reader.
type ScanFunc func(ctx context.Context, tbl catalog.Table) ([]lir.Row, error)

// Interpret evaluates a bound query the dumbest correct way, returning the
// same lir.Datum shape the real engine produces.
func Interpret(ctx context.Context, scan ScanFunc, q *bound.Query) (lir.Datum, error)
```

## Piece 2 — typed, schema-aware LIR generator

The hard, valuable part. **Generate from the type system downward**, not
arbitrary JSON that mostly discovers validation errors:

```text
catalog (one of many generated shapes)
  → populated tables (rows, with NULLs, dup values, empties)
  → relation nodes, each carrying its inferred output schema as it is built
  → expressions type-checked against the in-scope schema
  → a legal root cardinality / crossing shape
```

Correct-by-construction, not generate-then-filter. Each built node carries its
schema so children stay legal:

```go
type GeneratedRelation struct {
    Node   protocol.Node   // or unbound lir.Relation
    Schema []Column        // inferred output columns + nullability
    Scopes []ScopeName     // in-scope labels for correlation/refs
}

genExpr(scope, WantBool)        // for predicates / join on
genExpr(scope, WantInt64)       // arithmetic operands, etc.
genExpr(scope, WantComparable)  // order/group/compare
```

That yields thousands of **semantically valid** plans instead of thousands of
validator rejections. The generator must respect every binder rule or it just
tests the rejection path:

- unique scope labels across the whole query;
- an `order` wherever rows become observable (root `many`, `array` crossing,
  positive `slice.offset`); `first` needs order-or-provably-≤1; `scalar` needs
  one column and ≤1 row;
- **unique output names at every observable boundary** — auto-project joins to
  unique names (the bug we just fixed);
- crossings reference only in-scope outer columns; join sides can't see each
  other; `on` has no crossing;
- typed literals (int64 vs float64 by context; no numeric widening in
  comparisons this arc);
- aggregate/group term legality; column-above-aggregate resolves only to
  groups/terms.

**Many catalog shapes:** a small catalog generator emitting schemas with the
interesting features — nullable columns, a unique index, a composite index, a
self-referencing FK, two+ joinable tables, an all-NULL column, an empty table.
Seed the RNG from an explicit parameter (wall-clock/global RNG is unavailable in
some contexts and kills reproducibility); **log the seed on every failure.**

A separate malformed-input fuzzer (arbitrary JSON → the validator, asserting
clean rejection and panic-resistance) is a *different* campaign — worth doing,
not this ADR.

## Piece 3 — the harness: Case + Oracle + Features

Build the matrix around a generated **case**, with pluggable runners:

```go
type Case struct {
    Catalog  Catalog
    Data     map[TableName][]Row
    Query    lir.Query        // read-only for v1
    Features Features         // what semantics this case exercises
}

type Oracle interface {
    Execute(ctx context.Context, c Case) (Result, error)
    Supports(Features) bool
}
```

Runners for now: `RadOracle` (real engine) and `ReferenceOracle` (refexec).
`Features` (UsesLeftJoin, UsesNulls, UsesBindings, UsesAggregate, …) lets each
oracle declare its supported subset so the generator isn't reduced to the
intersection of every backend's limitations — future-proofing for when more
oracles (or the optimiser-on/off pair) join. `TestGeneratedDifferential`: loop
N cases; for each supported by both, bind once, execute both, compare.

Also add a standalone, **enumerated** (not generated) **3VL truth-table** test:
exhaustively cross `{TRUE, FALSE, UNKNOWN}` through `and`/`or`/`not` and every
comparison against a NULL operand, asserting the canonical Kleene result. Small,
total, and it is the independent pin for 3VL that lets the interpreter reuse
scalar eval (Piece 1).

## Piece 4 — metamorphic / property oracles

A second independent axis, no oracle needed. Each is a rewrite that must be
result-equivalent (subject to Rad's exact NULL/order semantics), checked by
running both forms through **both** the engine and the reference interpreter
(so a rewrite that breaks one path but not the other is caught):

- **Filter:** `filter(R, true) == R`; `filter(R, false) == ∅`;
  `filter(filter(R,p),q) == filter(R, p AND q)`.
- **Projection:** `project(project(R,[a,b,c]),[a,c]) == project(R,[a,c])`.
- **Join commutativity:** `R ⋈p S == S ⋈p' R` modulo column arrangement/scope
  names (inner); associativity where predicates make the rewrite valid.
- **Aggregate invariants:** `count(*) >= 0`; empty-input `sum`/`min`/`max` =
  NULL, `count` = 0 (Rad's spec); `sum(R ∪ S) == sum(R) + sum(S)` for
  non-overflowing numerics under the NULL rules.
- **Slice/order:** `slice(order(R),0,n)` is a prefix of `slice(order(R),0,n+1)`;
  ordered output actually satisfies the comparator.
- **Binding substitution (validates the binding rework):** a single-use `ref x`
  with `x = Q` equals inlining `Q` at the root; repeated `join(ref x a, ref x
  b)` equals two hygienically scope-renamed copies of `Q` — directly tests
  CTE-like reuse semantics.
- **(Later) optimiser equivalence:** `evaluate(unoptimised) ==
  evaluate(optimised)`, per-rule and end-to-end, once there is an optimiser.

## Shrinking → fixture-production machine

A raw generated failure (12 tables, 84 rows, 31 nodes) is nearly useless. The
harness must shrink toward: fewer tables/rows/columns/nodes, simpler predicates,
literals `0`/`1`/`NULL`, one binding. The shrunk case is then emitted
**automatically as a permanent `tests/e2e` fixture** (schema.rad + seed.json +
test_*.json) with a note recording the seed and which oracle/property caught it.
That turns property testing into a fixture-production machine — every failure
becomes a small, permanent, human-readable regression. The seed alone already
reproduces, so shrinking can land after the first differential test works.

## The layered oracle split

Route each failure to the abstraction boundary it broke, rather than one giant
"compare everything" test:

| Boundary under test | Oracle |
|---|---|
| Protocol/schema validity | JSON Schema + semantic validator (+ malformed fuzzer) |
| LIR meaning | naïve in-memory interpreter (this ADR) |
| Planner/optimiser correctness | interpreted original LIR vs interpreted rewritten; or engine opt-on vs opt-off (later) |
| Physical executor correctness | interpreter result vs Rad/KV result (this ADR, the core) |
| SQL frontend correctness | *(future [[sql-frontend]] — its output vs equivalent LIR)* |
| PIR mutation correctness | *(future — simple transactional state model vs real state)* |

## Comparison gotchas

- **Unordered results compare as sets, ordered as sequences.** Simplest: the
  generator appends a total unique-key order to every observable collection, so
  every comparison is a well-defined sequence.
- **Tie order is path-dependent** unless a unique key breaks ties; the engine
  appends a PK tie-breaker, so the interpreter must too (or the generator orders
  only by keys that include one).
- **NaN / non-total float compare:** exclude from generation or define
  explicitly.

## Sequence

1. Keep expanding explicit conformance fixtures (ongoing, orthogonal).
2. Promote `interp` → `rad/engine/refexec.Interpret`; rewrite
   `TestReferenceInterpreter` to use it. No behaviour change.
3. Add the enumerated 3VL truth-table test (cheap, independent, high value).
4. Typed schema-aware generator + catalog generator, starting tiny
   (scan/filter/project/order/slice) then joins (auto-projected) → aggregates →
   crossings/correlation → bindings.
5. `TestGeneratedDifferential` (Case/Oracle/Features) wiring the generator to
   the reference-interpreter differential + path-independence + batched≡nested;
   seed-logged, reproducible.
6. Shrinking + automatic `tests/e2e` fixture emission.
7. Metamorphic rewrites (filter/project/join/aggregate/slice/binding identities).
8. Later: reuse the machinery for optimiser-rule equivalence and
   [[recursive-queries]]; [[sql-frontend]] differential if ever wanted.

## Risks / honest critique (fold into sequencing)

- **Shared scalar eval blinds the differential to scalar bugs.** If both the
  engine and the interpreter call `bound.EvalDatum`, any bug *in* it is
  invisible to the differential — both compute the same wrong answer. This is
  not hypothetical: the int64-overflow-wrap and nondeterministic float→int-cast
  bugs (just fixed) lived in `bound.EvalDatum`; this oracle would **not** have
  caught them. The enumerated 3VL test only covers boolean logic, not
  arithmetic/cast/NULL-propagation. **Mitigation:** extend the enumerated
  scalar tests to cover arithmetic overflow, cast range, and NULL propagation
  at edge values (cheap, near-total) — and treat a fully independent
  interpreter evaluator as the real close-out, not a "nice to have."
- **The generator is the project; the interpreter is the warm-up.** The
  interpreter is small and mostly exists. Correct-by-construction typed
  generation with good self-coverage (measure which node/expr combinations it
  actually emits — an under-exploring generator gives false confidence) is where
  the effort and risk live. Don't mistake a finished interpreter for progress on
  the hard part.
- **Path-independence and batched≡nested are engine-vs-engine.** They cannot
  catch a bug consistent across access paths. The **interpreter differential is
  the load-bearing check**; the other two are cheaper supplements.
- **Same-starting-rows trap.** Rad coerces on insert (defaults, type coercion).
  Populate the oracle's in-memory tables from the rows Rad *actually stored*
  (insert, then read back), so a source-coercion difference can't masquerade as
  a query bug — one coercion, one source of truth.
- **Ordering vs. bag coverage.** Always appending a unique-key order (to compare
  as sequences) stops exercising the deliberately-nondeterministic bag paths
  (slice-without-order, encounter order). Compare unordered results as
  **multisets** (duplicates significant) instead, to keep bag semantics under
  test without imposing an order.

## Non-goals (this ADR)

- LIR → SQL / any second database.
- PIR mutation generation (read-only queries only for v1).
- Optimiser-equivalence oracle (no optimiser yet).
- A `rad verify` CLI.
