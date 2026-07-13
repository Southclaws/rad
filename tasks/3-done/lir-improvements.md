# LIR improvements

The work list distilled from the battle-test campaign
(docs/design/battle-test-findings.md, 127 tests, zero correctness bugs — all
findings are contract and developer experience). Each item records the
problem, the evidence, the fix, and its blast radius. None of the fixes
below change lir.schema.yaml: the grammar held; everything here is behavior
behind the schema. The one item that *would* change the grammar — F7 — is
deliberately design-pending.

Status: **done** (2026-07-13) — F1 through F6, F8, and F9 all shipped, one
commit each. F7 (DAG sharing) was split out: it is a grammar change and gets
its own design session — see tasks/1-todo/dag-sharing.md.

## F1 — Schema-validation errors must name the problem

**Problem.** A structurally invalid node is correctly rejected (400), but the
detail is the validator's raw dump: `oneOf: did not validate against any of
[<anonymous schema> ×7]`. Nothing names the node, the field, or the rule.
Binder errors set the bar (`planner: slice limit must be >= 0, got -2`); the
schema layer is nowhere near it.

**Evidence.** `TestWireNegativeLimitRejected`.

**Fix.** Kind-directed best-match in the validation wrapper
(rad/protocol/lirjson.go), no schema change: when validation fails, walk the
document's nodes; for each failing node that carries a recognisable `kind`,
re-validate it against the oneOf variant whose `kind` constant matches
(variants are addressable via the compiled schema's `OneOf` slice), and
report that variant's specific error with the node id:
`protocol: node "s": limit: must be >= 0`. Failures outside any node (bad
root, missing kind) fall back to the deepest single cause instead of the
whole tree.

**Blast radius.** lirjson.go + the wire tests that assert rejection shapes.

## F2 — The response envelope must carry a datum, not just records

**Problem.** `POST /query` responds `{"records": [...]}` — an array of
objects. A `scalar`-rooted query's naked value cannot ride that shape, so it
degrades to `[{}]`. This is the review document's "general datum envelope"
gap, observed in practice.

**Evidence.** Harness-era observation; the corpus avoids scalar roots.

**Fix.** The response becomes `{"result": <datum>}` where the datum renders
exactly as the root materialises: `many` → array of objects, `first` →
object or null, `exactly_one` → object, `scalar` → value or null. This is a
response-contract change in rad/api/openapi.yaml (freeform result), NOT in
lir.schema.yaml (which describes only the request).

Client surface: `Client.Query` keeps returning `[]protocol.Record` (array
result verbatim; object result wrapped as one record; null → empty) so
record-shaped callers and generated clients stay simple, and gains
`Client.QueryDatum(ctx, q) (any, error)` returning the decoded datum
verbatim — the honest form, and what scalar roots require. Generated Go/TS
runtimes switch their decode from `records` to `result`.

**Blast radius.** openapi.yaml + oas regen, server handlers, radclient,
codegen Go/TS runtime templates + demo regen, harness (Result gains the raw
datum + a datum assertion), new corpus tests for scalar/first roots.

## F3 — Error classification by type, not string prefix

**Problem.** The server decides 422-vs-500 by checking whether the error
string starts with `planner:` or `exec:`. It works, but it is fragile
(any wrapped error loses classification; any internal error that happens to
carry the prefix becomes client-visible) and it conflates two client-error
classes: bind-time rejection of the query (invalid) and runtime failure of a
valid query on the data it met (division by zero, exactly_one violation).

**Evidence.** `TestPredDivisionByZeroErrors`, `TestStressExactlyOneViolation`
(both observe 422/`invalid`).

**Fix.** Typed errors at the source, explicit mapping at the server:
- `planner.Bind` wraps everything it rejects in one `*planner.BindError`
  (single wrap point — the binder is the only producer). Maps to 422,
  code `invalid`, as today.
- Runtime evaluation failures — division by zero, cast/negate/compare
  domain errors, `exactly_one` violations — wrap in `*lir.RuntimeError`
  (the type lives in 03_lir because bound evaluation produces most of
  them). Maps to 422 with a new problem code `execution_failed` and type
  URI `urn:rad:problem:execution_failed`.
- The string-prefix classifier is deleted. Anything untyped is what it
  always should have been: a 500.
Message texts (and their `planner:`/`exec:` prefixes) stay — they are good
diagnostics; they just stop being the classification mechanism.

**Blast radius.** 03_lir/bound/eval.go, 04_planner/bind.go,
05_exec/execute.go, rad/protocol (new code constant), rad/server error
mapping, corpus tests asserting codes.

## F4 — Strict literal coercion should name the escape hatch

**Problem.** `stock > 1.5` → `planner: expected an int64 value, got "1.5"`.
Correct (no implicit promotion — a deliberate invariant), but the message
doesn't tell a SQL-reared user what to do instead.

**Evidence.** `TestPredFloatLiteralAgainstIntColumnRejected`.

**Fix.** Append the remedy when a fractional literal meets an integer
column: `— cast the column to float64 to compare against fractional values`.

**Blast radius.** 04_planner/bind_expr.go + the predicate test.

## F5 — Arithmetic and cast constructors in protocol

**Problem.** `protocol` has constructors for comparisons and logic but not
for `add/sub/mul/div`, `negate`, or `cast`, so every computed field is a
multi-line struct literal. The corpus grew a local `mul` helper within
minutes of existing; generated clients will need these too.

**Fix.** Add to rad/protocol/build.go: `Add/Sub/Mul/Div(l, r *Expr)`,
`Neg(e *Expr)`, `CastTo(e *Expr, kind string)`. Sweep the corpus's local
helper away.

**Blast radius.** build.go + corpus call sites.

## F6 — Say *why* a scope disappeared

**Problem.** Referencing a scope above the projection that closed it fails
with `planner: unknown scope "t"` — correct, and the single most-hit
authoring trap in the campaign (every author, human and agent, hit it).
"Unknown" is wrong in spirit: the binder knows the label existed and knows
what closed it.

**Fix.** The binder already tracks every label bound anywhere (query-wide
uniqueness). When resolution fails for a label that WAS bound but is no
longer visible: `planner: scope "t" is closed here — a projection or
aggregate above it established a new row type; label that node with a scope
and reference its output columns instead`.

**Blast radius.** 04_planner/bind_expr.go (resolution failure path) + a test.

## F7 — DAG sharing

Moved to tasks/1-todo/dag-sharing.md: a grammar change with semantic weight,
not a DX patch — it gets an explicit design session. The working idiom
(compute once in a labelled projection, reference the projection's scope
above) is documented on the LIR docs page.

## F8 — Idioms and footguns belong in the docs

Real-world behaviors that are correct but will surprise users; each needs a
short documented idiom rather than code:
- **Zero-row parents vanish from grouped folds.** Aggregate-of-aggregate
  over orders-per-customer averages over customers *with orders*. The
  spelling that keeps zero-row parents: LEFT join + `count(non-null col)`
  (`TestJoin_LeftJoinCountKeepsZeroCustomers`).
- **Text primary keys order lexicographically**: `"i10" < "i9"`.
- **Raw join rows are never flattened**: spreading both sides of a join
  collides on shared column names (`duplicate projection field "id"`);
  project explicitly.
- **Order-then-project**: ordering above an unlabelled projection cannot see
  the input's scopes; order below the projection or label it (see F6, F7).

**Fix.** An "Idioms" section in the LIR docs page (home/content/docs/lir.mdx).

## F9 — Corpus infrastructure

- The corpus runs serially (~0.9s/test, dominated by store boot + commit
  flush). The harness is parallel-safe; turn on `t.Parallel()` across
  tests/planner before the corpus grows past the pain threshold.
- `task test -- <pkg>` always builds and tests the whole module; add a
  `task test:planner` shortcut for the corpus loop.
- The Go client normalizes node structs by `kind` before sending, so
  malformed unions can only be probed by raw HTTP (wire_test.go does this
  deliberately). Keep both surfaces in mind when adding validation tests.

## Sequencing

1. F5 constructors (minutes) → 2. F4 + F6 binder messages → 3. F1 validation
best-match → 4. F3 typed errors → 5. F2 datum envelope (bundled with the
response-shape work the explain decoration wants anyway) → 6. F8 docs +
F9 infra. F7 waits for an explicit design session.
