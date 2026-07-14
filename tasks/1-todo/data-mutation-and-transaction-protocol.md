# Execution programs (PIR): data mutation and the transaction protocol

Status: settled design, third revision (2026-07-14). Revision one planned
incremental hardening of the CRUD + `/tx` surface; revision two replaced it
with the execution-programs ADR grounded in the codebase; this revision
folds in the design review that settled the open questions. The layer has a
name: **PIR — Program Intermediate Representation**. Ready to build;
staging plan at the end. Grounding notes are marked ▸.

## The kernel (normative)

> A PIR program is an ordered array of named statements executed
> sequentially within one implicit transaction. Each statement evaluates
> against the transaction's initial snapshot plus the complete effects of
> all preceding statements, but never observes its own effects while
> deriving its mutation input. Statements may expose their result relation
> as a materialised program binding for later statements. Mutation
> statements validate and apply their complete input relation as one
> logical set. If any statement or the transaction commit fails, the
> entire program fails and no effects become externally visible. Exactly
> one statement result is returned to the caller, defaulting to the sole
> statement when the program contains only one.

The design removes three sources of accidental nondeterminism at once:
unordered statement scheduling, per-row constraint checking over bags, and
replayable effectful references. It is not merely an API redesign; it is a
better statement semantics model.

## The layering

```text
HTTP
  │
PIR — execution program  (effectful orchestration; owns ordering + atomicity)
  │
Statements               (query | create | update | delete)
  │
Pure relation LIR        (no side effects, no execution-order semantics)
  │
Planner → physical plan → KV operations
```

LIR stays a pure relational language; PIR introduces effects one level
above it, exactly as the planner sits above LIR today. Alongside the
schema IR (catalog) the pattern completes: each layer has one
responsibility and compiles into the next.

The transport collapses to one data-plane endpoint, **`POST /execute`**.
`/query`, `/create`, `/update`, `/delete`, and `/tx/{id}/...` are removed —
**decided: `/query` does not survive as a shorthand, and no second
wire-level grammar normalises into a one-statement program.** One protocol
model, one validation path, one trace shape, one error model, one
transaction lifecycle; ergonomics are the job of client libraries, a
`rad query` CLI frontend, and the devtool making PIR pleasant — not of a
sibling endpoint that would grow into the weird restricted cousin.

▸ Grounding: the layering matches the engine as built. `frontend.Tx.Execute`
already binds and executes statements against a transaction's view
(snapshot + own writes), so read-your-writes exists at the engine layer
today; the program executor is a loop over statements inside one
`engine.Txn`. The statement snapshot is already a stated invariant. And the
relation-bindings kernel was deliberately worded "a binding denotes one
_statement-local_ relational value" — PIR makes "statement" a first-class
construct and that sentence survives verbatim.

## What this dissolves (from revision one)

- **Interactive sessions and everything they dragged in.** No `/tx` means
  no session registry, no lease/expiry contract, no ambiguous-commit
  terminal states, no owner-routing rule: every request is a complete
  atomic unit, so Rad becomes stateless-per-request — a deployment story
  win, not just an API cleanup. Interactive transactions return only if a
  workflow genuinely needs application work between statements while
  holding a snapshot.
- **The transaction-context mechanism** (header vs envelope): moot.
- **The bounded batch IR** (`Mutate([]Mutation)` with no dataflow):
  superseded — programs are the batch, with dataflow.
- **The `set`/`clear` overlap edge case**: dies structurally (update
  rules below).

## Wire shape

```jsonc
{
  "statements": [
    { "name": "author",
      "kind": "create", "table": "users",
      "input": { "nodes": { /* … */ }, "root": "..." } },
    { "name": "posts",
      "kind": "query",
      "query": { "nodes": { /* … */ }, "bindings": { /* … */ }, "root": { /* … */ } } }
  ],
  "result": "author"
}
```

Statements are an ordered **JSON array** of named entries — never a map.
Query.nodes can be a map because node order is meaningless; statement order
is semantic and JSON objects are unordered. The array is also the DAG
answer: a list of named statements whose references must point backwards is
a topologically-sorted DAG, so the wire is DAG-capable while the executor
stays linear.

`protocol.Query` (nodes/bindings/root) survives intact as the statement
payload; PIR is the new outer document. The LIR format, schema, validation,
and graphconv do not change beyond the `rows` node below.

### PIR is a separate spec on the same pipeline (normative)

PIR gets its **own JSON Schema YAML document**, not a section of the LIR
schema — the correct expression of the layering: LIR is the pure relational
language, PIR the effectful layer above it, two independent specs that ride
the same codegen pipeline. Mirror the LIR wiring exactly:

```text
pir.schema.yaml   (authored source of truth)
  → schemagen  →  pir.schema.json  +  home/public/schema/pir.json (web copy)
  → schemancer →  rad/protocol/pirwire  (generated wire types)
pirjson.go        ergonomic Program/Statement types + Marshal/Unmarshal
pirvalidate.go    best-match validation, mirroring lirvalidate.go
```

`task protocol:generate` grows the PIR steps beside the LIR ones; a
`pir.schema.yaml`-shaped `schemancer` mapping reuses the same `format: raw`
→ `json.RawMessage` trick.

**The two schemas stay independent — no cross-document `$ref`.** Each
statement's LIR payload is an opaque `format: raw` field in the PIR schema
(`json.RawMessage` in `pirwire`). Validation is two-phase, exactly as
OpenAPI already treats `/query` bodies: the PIR schema validates the
envelope and statement grammar (names, kinds, `result` selection, array
shape), then the **existing** `ValidateLIRJSON` validates each statement's
raw LIR document. One validator per document; PIR never learns the LIR
grammar and LIR never learns it has a parent. Statement-result bindings —
the one place PIR reaches into LIR semantics — are resolved at bind time in
the engine (a `ref` to a program-scope name), not in the schema.

## Ordering (normative)

> A program is an ordered array of statements. Statements execute
> sequentially in document order within one transaction.

- Statement-result references may only point backwards; forward references
  are invalid; cycles are unrepresentable.
- Every statement sees the effects of all earlier statements.
- No topological sort is required for correctness; no deterministic
  tie-breaking question exists.
- The v1 planner simply preserves statement order. Reordering or
  parallelisation is a future optimisation that requires proof of
  observational equivalence — covering at least read/write table
  footprints, constraint interactions, generated/default expression
  effects, statement-result references, future trigger/hook effects, and
  catalog reads where relevant. Even disjoint tables may be insufficient
  once foreign keys connect them, so the proof system is deliberately not
  specified narrowly here; the bar is "invisible by construction".

▸ This replaced the original "order derived from dependencies" model: refs
are not the only dependencies — statements also interact through tables
(a `create` into `users` followed by a ref-independent `query` over
`users` must observe the insert), so ref-graph scheduling was wrong.

## Statement lifecycle (normative)

For every mutation statement, the logical sequence is:

```text
1. Evaluate the complete input relation against the statement start state.
2. Derive the complete proposed mutation set.
3. Validate the proposed post-statement state.
4. Apply the complete mutation set.
5. Produce the statement result relation.
```

The statement start state is:

```text
program transaction snapshot
+ effects of statements 0..n-1
= start state of statement n
```

**The Halloween rule falls out**: a statement's relations evaluate against
its start state, so an update whose input reads its own target table cannot
observe its own writes. Read-your-writes gets its precise wording:

> Each statement executes against the transaction's snapshot plus the
> effects of all _preceding_ statements — never its own.

**Constraint validation happens at step 3, against the proposed
post-statement state** — not per row in encounter order. Today `mutate.go`
checks per row immediately; over an unordered bag that makes failure order
nondeterministic. Statement-boundary validation is a real semantic upgrade:

- A create statement may contain mutually-referential rows (an `employees`
  batch whose `manager_id` values point inside the batch) — valid if the
  post-statement state satisfies the FK.
- A single update statement may swap unique values (A and B exchange
  usernames) — valid because only the end state is checked.

Intra-batch duplicates (two input rows minting the same PK) are caught at
the same step. Across statements the current model stands: constraints are
immediate at each statement boundary, none are deferrable to commit,
restrict remains the only FK delete action.

**Two validation layers, two error categories** (already settled taxonomy,
commit `350058c`): statement constraint validation against the
transaction-visible state plus the proposed delta → `invalid` (retry
cannot help); transaction commit validation against concurrent activity →
`conflict` (immediate retry of the whole program may win). A failed
statement rolls back the whole program and the error names the failing
statement.

## Statement results are bindings (normative rules)

Statement names enter a program-level binding namespace; later statements'
LIR consumes earlier results with the ordinary `ref` node — no
`statement_ref`, no second reference system. The rules:

1. Statement names are unique within the program.
2. A statement name becomes bound only after that statement succeeds.
3. References resolve only to preceding statements.
4. Statement-result bindings are logically materialised and evaluated
   exactly once — **never replayed**; mutations are not re-executable, so
   the single-ref replay strategy is off the table at program scope.
5. Statement-local bindings may not shadow program statement names —
   rejected outright, so resolution never distinguishes "local relation
   binding" from "program result binding".
6. Forward references are invalid.
7. The result statement's output remains available for the program
   response after commit.

"Materialised" is logical, not necessarily RAM: the executor may buffer,
spool, retain an iterator under safe conditions, or reconstruct from an
internal write set — anything except re-running the producing statement.

## Statement semantics

**Create.** `input`'s output columns map to target columns by name;
missing columns take defaults (generators applied per row, as today);
unknown columns are an error; types must be assignable per the catalog.
Result relation: the created rows, defaults included.

**Update.** The input relation's _schema_ is the assignment contract: for
`users(id PK, name, age)`, an input of `id | name` means "identify by id,
assign name"; `id | age` means "identify by id, assign age". NULL assigns
null; absence from the schema means leave unchanged. No `set`, no `clear`,
no overlap validation, no per-row patch objects, no mutation-specific
assignment language. The set of assigned columns is fixed by the input
schema for the entire statement — not an implementation limit but what
makes the statement statically understandable; different update shapes are
separate statements, which PIR makes cheap and atomic. Validation rules:

- all primary-key columns must appear; PK columns identify and are not
  assignable in v1 (immutable keys carry over);
- no non-target columns may appear;
- generated/non-writable columns are rejected if present;
- input column types must be assignable to target columns;
- the input schema must be statically known before execution;
- **every target identity may occur at most once in the input bag** — two
  rows targeting the same PK are an ambiguity error even if they assign
  identical values (relations are bags; last-wins is not a thing);
- **every input row must identify exactly one existing target row** — a
  miss fails the statement and therefore the program. Update is
  declarative ("these are the rows to update"), not aspirational ("update
  whichever happen to exist"). Distinct reasons for the error registry:
  `mutation_target_not_found`, `mutation_target_ambiguous`.

Result relation: the post-image of updated rows.

**Delete.** Input's output must be exactly the target's primary-key
columns (extra columns rejected). Same strictness as update on misses in
v1, for consistency — when the input derives from scanning the target
itself, misses cannot naturally occur anyway. A tolerant
`missing: ignore` statement option is conceivable later, not in v1;
skip-and-report is a different mutation contract and makes downstream
result cardinality unpredictable. Result relation: the pre-image of
deleted rows — the `returning` story for free.

**Query.** Unchanged LIR semantics; root cardinality shapes the result as
today.

## Result model

Two different concepts: statement relational outputs (for composition) and
the program transport output. Only the declared `result` statement's
relation crosses the wire — a query-driven delete may touch 100k rows and
shipping every relation punishes exactly the workloads programs exist for.

```jsonc
{
  "statements": [
    { "name": "create_users", "affected": 10 },
    { "name": "delete_legacy", "affected": 100000 }
  ],
  "result": {
    "statement": "create_users",
    "relation": { /* datum, shaped by the statement's cardinality */ }
  }
}
```

- `result` selection: omitted ⇒ the sole statement of a one-statement
  program; multi-statement programs must name it explicitly. **Never
  default to the last statement** — appending an audit statement must not
  silently change the API response.
- Whether every successful statement earns an envelope entry (vs counts
  living only in trace/debug output) is deliberately open; start with the
  lightweight counts and let the trace work own the rich version.
- Old/new both for updates: deferred; if change capture is wanted, model
  pre/post images as explicit relations, not flags.

## Prerequisite: LIR grows the constant relation — LANDED

**PIR implementation depends on constant relation support in LIR.** The
`rows` node shipped 2026-07-14 as stage 1 of the cutover: the second
relational leaf beside `scan`, through the whole vertical (schema YAML →
schemagen → lirwire → validate → graphconv → bind → plan → execute →
oracle), with corpus and conformance coverage.

```jsonc
{ "kind": "rows", "scope": "r",
  "columns": [
    { "name": "name", "type": "text" },
    { "name": "age",  "type": "int64", "nullable": true }
  ],
  "rows": [["ada", 36], ["grace", null]] }
```

Design as landed (after review): column **types and nullability are
declared, never inferred** — a relation's schema must not depend on its
data, an empty `rows` stays fully typed, and a NULL cell in a
non-nullable column is invalid. Cells use the protocol `Value` encoding
and are validated and decoded against the declared column type under the
same rules as scalar literals. The relation is a bag of exactly
`rows.length` rows; bag contents are deterministic and independent of
plan choice, but document order is not a logical order — `order` above
makes it observable. Cardinality is exactly the row count, so a one-row
constant satisfies `first` with no order.

## What survives from revision one

- **Prove the isolation claim at the storage seam** — more urgent now:
  programs put multi-statement read-your-writes at the centre of the
  contract. Write-skew (two-doctors), phantom ranges, constraint races
  (duplicate unique values, FK parent deletion), backend conformance
  beyond in-memory SlateDB.
- **Idempotency stays open.** Programs do not solve the lost-response
  problem — a client that never hears back from `/execute` cannot know
  whether the program committed. Client-supplied idempotency key with a
  bounded result record remains the likely mechanism, and matters more
  when one request carries a whole workflow.
- Immutable primary keys, composite-PK identification, and documented
  generated-defaults semantics carry over as statement rules.

## Cutover (zero-legacy, staged)

```text
1. LIR rows node, end to end (independent, pure, corpus-tested). LANDED.
2. PIR spec + validation: `pir.schema.yaml` on the LIR pipeline (schemagen
   → `pirwire`, `pirjson.go`, `pirvalidate.go`), envelope + statement
   grammar; opaque raw LIR payloads validated by the existing LIR
   validator (see "PIR is a separate spec" above).
3. Ordered program planning and the statement binding scope.
4. Query statements through /execute.
5. Create/update/delete statements through /execute.
6. Switch generated clients and harness — MINIMAL port only (below).
7. Delete old endpoints, explicit transactions, sessions, and old
   mutation request types. No compatibility aliases.
```

Each stage is testable without maintaining public compatibility.
`task demo` and `task demo:ts` remain the acceptance gates.

▸ Grounded inventory:

- The battle-test corpus (~150 tests) and `tests/harness` speak
  `POST /query` via radclient; the harness `Result` seam absorbs the
  envelope change in one place. Harness `Insert`'s txn-batching workaround
  becomes one `rows`-fed create statement in one program — strictly
  better, and it tests the intended execution path.
- `client.Txn`/`Begin` are used by the generated Go client, the TS
  runtime, the harness, and `tests/e2e_test.go`.
  **Generated clients: do the minimum to compile and keep the demo
  running — no redesign this arc.** The client/codegen world gets its own
  rethink later (big separate todo). The one semantic note to carry into
  that rethink: a program-building callback is _construction_, not live
  interaction — callback code cannot branch on actual query results, so
  `Txn(fn)` ergonomics translate only partially and the name itself
  (`Program`/`Atomic`/`Execute`?) is part of that future discussion.
- `rad/server/api/sessions.go` and the `/tx` handler tree delete;
  dbserver_test's transaction cases become program cases.
- Devtool UI/keydecode are read-path, untouched; the catalog API
  (direct-catalog-mode) is control-plane, untouched.

## Cross-ADR effects

- **error-propagation.md**: `Location` gains `Statement`; the stage list
  likely gains `program` (namespace/reference validation is neither schema
  nor binding). New reasons: `mutation_target_not_found`,
  `mutation_target_ambiguous`. Statement-constraint vs commit-conflict
  remain distinct classes.
- **query-trace.md**: the trace becomes program-scoped — statement spans
  at the top, per-statement planning/KV sections beneath; "an error is a
  trace that stopped early" stops at a statement boundary. Per-statement
  affected-counts may live richer here than in the response envelope.
- **relation bindings** (tasks/3-done/relation-bindings.md): the kernel
  extends unmodified to program scope; the one new rule is that
  statement-result bindings always materialise.

## Remaining open questions (small)

1. Per-statement metadata in every successful response, or counts in the
   envelope + rich detail only in traces? (Start light; trace work owns
   the rest.)
2. Program limits: statement count, total input rows, result size — the
   execution-limits deferral gains an axis; bound something before demos
   get creative.
3. Result selection as a list (named-results object) — backwards-
   compatible extension if ever wanted; start with one.

## Non-goals

- Replacing HTTP/JSON; HTTP/2 + programs-as-batch is the early
  optimisation; streaming/alternate codecs later, same semantics.
- Distributing live transactions — programs deliberately make requests
  stateless instead.
- Upsert, cascades, deferred constraints, tolerant misses, or an
  expression-assignment UPDATE dialect (`set x = x + 1` is a query
  feeding an update — that _is_ the design).
- Interactive transactions — only with a workflow that genuinely needs a
  held snapshot across think-time.
- Generated-client redesign (separate future task; this arc only keeps
  them compiling).

Related: tasks/3-done/relation-bindings.md (the reference machinery),
tasks/1-todo/error-propagation.md and query-trace.md (program-scoped
provenance), tasks/1-todo/typeless-value-encoding.md (row literals),
tasks/1-todo/enforced-ordering-in-certain-contexts.md (mutation input
determinism), rad/engine/06_frontend (Tx.Execute — the read-your-writes
seam that makes the executor a loop).
