# LIR battle-test findings

Status: report of the 2026-07-13 battle-test campaign. The work list lives
in tasks/: F1–F6, F8, F9 are fixed (tasks/3-done/lir-improvements.md); F7
(DAG sharing) awaits an explicit design session
(tasks/1-todo/dag-sharing.md). This document remains the record of what the
campaign covered and found.

## What was done

127 end-to-end tests in `tests/planner/`, every one a literal wire tree sent
through the real client, HTTP server, schema validation, binder, planner, and
executor, asserting exact rows back (106 exact-equality assertions; the rest
deliberate error and emptiness probes). No codegen anywhere. Two documented
fixtures (a task tracker and a five-table shop with a self-referential FK,
nullable columns, composite indexes, and all four scalar types) with every
seed row written in the fixture header so each expectation is hand-derived.

Coverage by theme:

| File | Tests | Territory |
|---|---|---|
| planner_test.go | 10 | exemplar patterns, error contract |
| basic_test.go | 18 | scan/filter/project/order/slice fundamentals |
| predicates_test.go | 17 | three-valued logic, arithmetic, casts, coercion |
| nesting_test.go | 22 | crossings: nesting to 4 levels, correlation, top-N per parent |
| joins_test.go | 15 | inner/left/self/theta joins, join+aggregate, rejections |
| aggregates_test.go | 16 | folds, GROUP BY, HAVING, typing, empty sets |
| stress_test.go | 20 | compositional depth, preflight structural rejections |
| wire_test.go | 8 | strict schema, malformed payloads, int64 precision, scale |
| (harness exemplars) | 1 | — |

## The headline

**Zero correctness bugs.** Every hand-derived expectation matched, including
the shapes designed to break the planner:

- Six-operator chains; 60-node filter towers; 61-term conjunctions.
- Four-level nesting (customer → orders → items → product) with per-level
  filters and orders; top-N per parent; grandparent (skip-level) correlation;
  crossings containing joins; joins of crossing-bearing projections;
  aggregate-of-aggregate; crossings as order terms and filter predicates,
  wrapped in arithmetic and comparisons.
- K3 three-valued logic held everywhere probed: De Morgan over NULLs,
  NULL-literal equality, `ne`/`not(eq)` null-skipping, OR over nullables,
  padded left-join columns UNKNOWN in ON, folds skipping padded NULLs.
- Aggregation typing exactly per contract: count 0 / others NULL on empty,
  avg always float, sum keeps int64, bool group keys, computed group
  expressions, HAVING as filter-above-aggregate.
- Determinism enforcement had no false positives or negatives across
  PK-equality, unique-index equality, ungrouped folds, explicit orders.
- The strict preflight rejects cycles, dangling refs, shared nodes, and
  unreachable nodes with messages that name the offending node.
- int64 values above 2^53 survive the full round trip.
- Pagination is stable and disjoint under the order + unique tie-break rule.

The semantic-foundation work (statement snapshots, datum unification,
crossing extraction, the oracle) appears to have paid for itself: the corpus
could not produce a wrong answer.

## Findings

Everything found is contract/DX, not correctness. In priority order:

### F1 — Schema-validation errors are unusable (highest value, small fix)
A malformed node (e.g. `"limit": -1`) is correctly rejected with 400, but the
detail is the raw validator dump: `oneOf: did not validate against any of
[<anonymous schema> ×7]`. Nothing names the node, the field, or the rule.
Evidence: `TestWireNegativeLimitRejected`.
**Fix**: title each union variant in lir.schema.yaml and implement a
best-match heuristic (validate against the variant matching `kind`, report
that error) in the validation wrapper. Binder errors are excellent; the
schema layer should meet that bar.

### F2 — Scalar-rooted queries are lossy on the wire
Root cardinality `scalar` executes correctly but `datumRecords` can only
carry objects, so the value comes back as `[{}]`. Known since the harness was
built; the corpus works around it by never using scalar roots.
**Fix**: the response envelope needs a datum root (`{"datum": ...}` or
similar) instead of records-only — this is the review document's "general
datum envelope" item; do it alongside the explain decoration since both touch
the response shape.

### F3 — Runtime evaluation errors are 422 by prefix accident
`exec: division by zero` and `exec: expected exactly one row` map to 422
because the server classifies client errors by the `planner:`/`exec:` string
prefix. A data-dependent runtime failure labelled "invalid request" is
debatable; the prefix-string mechanism itself is fragile.
Evidence: `TestPredDivisionByZeroErrors`, `TestStressExactlyOneViolation`.
**Fix**: typed error classes (sentinel errors or an error-code type) from
binder/executor, mapped explicitly to problem codes; distinguish
`invalid` (bind-time) from `failed_precondition`-style runtime codes.

### F4 — Strict literal coercion doesn't name the escape hatch
`stock > 1.5` → `planner: expected an int64 value, got "1.5"` — right
behavior, but users coming from SQL's implicit promotion will hit this
constantly and the message doesn't mention `cast`.
Evidence: `TestPredFloatLiteralAgainstIntColumnRejected`.
**Fix**: extend the message: "…cast the column to float64 to compare against
a fractional value".

### F5 — No arithmetic/cast constructors in protocol
`protocol.Eq/Col/Lit` exist but Add/Sub/Mul/Div/Cast/Neg don't, so every
computed field is a three-line struct literal; the corpus grew a local `mul`
helper immediately.
**Fix**: add the missing constructors to `rad/protocol/build.go` — they're
production vocabulary (codegen emits arithmetic eventually), not test sugar.

### F6 — Scope closure above unlabelled projections is the #1 authoring trap
Ordering or filtering above an unlabelled projection by the *input's* scope
fails with `unknown scope` — correct semantics (the projection closed it),
and every author (human and agent) hit it at least once. The error is
accurate but doesn't say *why* the scope disappeared.
**Fix**: when a scope name failed to resolve but was bound *somewhere below a
projection boundary*, say so: `scope "t" is closed by the projection at node
"out"; label the projection and reference its output`. Pure binder DX.

### F7 — Single-consumer forces subtree duplication for order-by-and-project
To order by a crossing value *and* project it, the query must either
duplicate the whole crossing subtree under fresh ids/scopes or restructure as
label-project-then-order-by-projected-column. The latter works well (the
corpus uses it), but the restriction will surprise query compilers.
**Fix**: none now — this is the documented DAG-sharing gap; the planned
`Let`/binding construct addresses it. Record the restructuring idiom in the
LIR docs as the recommended pattern.

### F8 — Real-world footguns that need documentation, not code
- Grouped aggregates drop zero-row parents: avg-orders-per-customer computes
  over 4 groups, not 5 customers (`TestStressAggregateOfAggregate`); the
  LEFT-join + countOf idiom (`TestJoin_LeftJoinCountKeepsZeroCustomers`) is
  the correct spelling and belongs in the docs.
- Text primary keys order lexicographically: `"i10" < "i9"`.
- Raw join rows can't be spread from both sides when column names collide
  (`duplicate projection field "id"`) — by design (no flattened joins), but
  the docs should show the explicit-projection idiom.

### F9 — Test-infrastructure observations
- The Go client normalizes Node structs by `kind` before sending, so
  malformed unions can't be produced through it — server validation is only
  reachable by raw HTTP (wire_test.go does this). Fine, but worth knowing:
  client-side and server-side validation surfaces differ.
- Suite cost is ~0.9s/test, dominated by per-test store boot and the ~100ms
  SlateDB commit flush (already mitigated by one-transaction seeding). At
  1,000 tests this is ~15 minutes serial: enable `t.Parallel()` in the corpus
  (the harness is already parallel-safe) and/or share read-only fixtures
  per-file before it hurts.
- `task test -- <pkg>` builds and runs the entire module every time;
  a `task test:planner` shortcut would tighten the loop.

## Suggested sequencing

1. F5 (constructors) + F4 (message) — trivial, immediate authoring relief.
2. F1 (schema error best-match) — biggest DX win per line of code.
3. F3 (typed error classes) — do before the error surface calcifies further.
4. F2 (datum envelope) — bundle with the explain decoration work, which
   wants a response-shape change anyway.
5. F6 (scope-closure hint) — small binder change, large first-hour UX gain.
6. F8 (docs) — fold into the LIR docs' "idioms" section.
7. F7 — wait for the binding construct; document the idiom now.

## What this says about LIR

The goal was to test whether LIR can express complex real-world business
queries. It can: dashboards (counts, revenue rollups, top-N per group),
detail pages (multi-level nesting with per-level shaping), search (dynamic
conjunctions, ranges, null-safe negation), reporting (grouped folds over
joins, HAVING, aggregate-of-aggregate), and referral/graph-ish shapes
(self-joins, skip-level correlation) all compose from the four primitives
without special cases — and, more importantly, without wrong answers. The
gaps are all at the edges: error ergonomics, the response envelope, and the
documented absence of DAG sharing.
