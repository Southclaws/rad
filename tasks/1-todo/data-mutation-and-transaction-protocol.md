# Execution programs: data mutation and the transaction protocol

Status: proposed ADR, second revision (2026-07-14). The first revision of
this task planned incremental hardening of the CRUD + `/tx` surface; the
execution-programs ADR supersedes most of it. This revision integrates the
ADR with the codebase as it exists, adds concrete proposals for its open
questions, and keeps the parts of the old plan that still bind. Grounding
notes and proposals beyond the original ADR text are marked ▸.

## Decision (proposed)

Introduce **execution programs** as the top-level execution abstraction:
one `POST /execute` accepting a bounded collection of statements executed
atomically inside one implicit transaction. Statements are `query`,
`create`, `update`, `delete`. Mutations consume *relations*, not literal
row payloads — a literal row is a one-row relation. The `/query`,
`/create`, `/update`, `/delete`, and `/tx/{id}/...` endpoints are removed.

The layering — the strongest outcome of this design:

```text
HTTP
  │
Execution program        (effectful orchestration; owns ordering + atomicity)
  │
Statements               (query | create | update | delete)
  │
Pure relation LIR        (no side effects, no execution-order semantics)
  │
Planner → physical plan → KV operations
```

LIR stays a pure relational language; programs introduce effects one level
above it, exactly as the planner sits above LIR today. Alongside the schema
IR (catalog) this completes the pattern: each layer has one responsibility
and compiles into the next.

▸ Grounding: the layering is not aspirational — it matches the engine as
built. `frontend.Tx.Execute` already binds and executes statements against
a transaction's view (snapshot + own writes), so read-your-writes exists at
the engine layer today; the program executor is a loop over statements
inside one `engine.Txn`. The statement snapshot is already a stated
invariant (one statement, one KV view). And the relation-bindings kernel
was deliberately worded "a binding denotes one *statement-local* relational
value" — programs make "statement" a first-class construct, and that
sentence survives verbatim. The pieces were built for this shape before the
shape had a name.

## What this dissolves (from revision one)

Programs make several planned decisions unnecessary — worth naming, since
it is most of the win:

- **Interactive sessions and everything they dragged in.** No `/tx`
  endpoints means no session registry, no lease/expiry contract, no
  ambiguous-commit terminal states, no owner-routing or deployment-honesty
  rule: every request is a complete atomic unit, so Rad becomes
  stateless-per-request — a big deployment story win, not just an API
  cleanup. Interactive transactions return later only if a real workflow
  needs application work *between* statements while holding a snapshot.
- **The transaction-context mechanism** (header vs envelope): moot, no
  transaction outlives a request.
- **The bounded batch IR** (`Mutate([]Mutation)` with no dataflow):
  superseded. Programs are the batch, with dataflow — the thing revision
  one deferred is now the design's centre.
- **The `set`/`clear` overlap edge case**: dies structurally; see the
  update statement rules below.

## Statement model and wire shape

▸ Proposal (resolves open questions 1 and 6 together):

```jsonc
{
  "statements": [
    { "name": "author",
      "kind": "create", "table": "users",
      "input": { "nodes": { ... }, "root": "..." } },
    { "name": "posts",
      "kind": "query",
      "query": { "nodes": { ... }, "bindings": { ... }, "root": { ... } } }
  ],
  "result": "author"
}
```

- **Statements are an ordered JSON array of named entries, not a map.**
  Query.nodes can be a map because node order is meaningless; statement
  order is semantic (below), and JSON objects are unordered. The array *is*
  the DAG answer: a list of named statements whose references must point
  backwards is exactly a topologically-sorted DAG, so the wire is
  DAG-capable while the executor stays linear — the ADR's "don't decide
  yet" position, made concrete.
- **Statement results are program-level relation bindings.** Each
  statement's name enters a program binding namespace; later statements'
  LIR trees consume earlier results with the ordinary `ref` node — no new
  reference construct. This is the binding kernel doing its job: a
  statement result is one committed relational value, observed under fresh
  scopes by any number of refs. One rule carries over hard: statement
  results are **always materialised, never replayed** — mutations are not
  re-executable, so the single-ref replay strategy is off the table here.
- **Namespaces**: each statement's LIR document keeps its own local
  `bindings`; `ref` resolution is local-first, and a local binding that
  shadows a statement name is rejected outright (deterministic, no lookup
  ambiguity). Statement names, node ids, binding names, and scope labels
  remain four separate namespaces.

## Ordering: document order is the semantics

▸ Pushback on the ADR's "execution order is derived from dependencies":
ref-edges are not the only dependencies. Two statements with no reference
between them still interact through *tables* — `create` into `users`
followed by an unrelated-looking `query` over `users` must observe the
insert. A scheduler that derives order from the ref graph alone would be
free to reorder them and change results; deriving table-footprint edges is
possible (the binder knows each statement's read/write sets) but is planner
sophistication with no v1 payoff.

Proposal: **document order is the semantic execution order.** Validation
requires references to point to earlier statements (the user hands us a
topological order; cycles are unrepresentable). A future planner may
reorder or parallelise only where it can prove independence on *both* the
ref graph and table footprints — an optimisation invisible by construction,
which is the only kind allowed. This also deletes the deterministic
tie-breaking open question: there is nothing to tie-break.

## Statement semantics

▸ Proposals for the rules the ADR leaves open:

**Statement-internal snapshot (the Halloween rule).** Every relation in a
statement evaluates against the program state as of the statement's start;
the statement's effects become visible only to subsequent statements. So an
update whose input reads its own target table cannot observe its own
writes, and read-your-writes gets its precise wording for free:

> Each statement executes against the transaction's snapshot plus the
> effects of all *preceding* statements — never its own.

This is the engine's existing statement-snapshot invariant applied
per-statement inside one transaction; the machinery exists.

**Create.** `input`'s output columns map to target columns by name;
missing columns take defaults (generators applied per row, as today);
unknown columns are an error; types must match the catalog (binder
validation, same as everywhere else). Result relation: the created rows,
defaults included — the wire generalisation of today's `Create` returning
the stored row.

**Update.** The input relation's *schema* declares the assignment: its
output must include the target's full primary key (identifying which rows
change) and at least one non-key column (the columns being set). Columns
absent from the input schema are untouched; a NULL in a nullable input
column assigns NULL. This kills the `set`/`clear` split — "clear" is just
projecting a NULL — and the old silent-overlap bug with it. Rules that
need stating: a PK value that matches no row is an error or a skip
(propose: error — programs are atomic, silent partial application is
poison); two input rows targeting the same PK are an error (deterministic,
no last-wins). Result relation: the post-image of updated rows.

**Delete.** Input's output must be exactly the target's primary key
columns (extra columns rejected — say what you mean). Result relation: the
pre-image of the deleted rows (their last committed state), which is the
only useful answer and doubles as the `returning` story.

**Constraint timing moves to the statement boundary.** Today `mutate.go`
checks constraints per row, immediately. A mutation statement consumes a
*bag* — unordered by definition — so per-row immediate checking would make
failure order nondeterministic, and self-referential rows created in one
statement (an `employees` batch with `manager_id` pointing inside the
batch) would fail on encounter order. Rule: a statement's constraint
checks (unique, FK, NOT NULL) run against the statement's *end* state,
after all its rows apply; intra-batch duplicates are caught there too.
Across statements the current model stands: constraints are immediate at
each statement boundary, none are deferrable to commit, restrict remains
the only FK delete action.

## Result model

▸ Proposal (resolves open questions 2 and 5):

- Every statement logically produces a relation (consumable via refs), but
  **only the program's declared `result` statement materialises to the
  wire** — a delete driven by a query may touch 100k rows, and shipping
  every statement's relation by default punishes exactly the workloads
  programs exist for. The response envelope carries the result datum
  (shaped by the result statement's cardinality, `{"result": ...}` as
  today) plus a small per-statement summary (affected-row counts) that the
  trace work can later enrich.
- `result` is explicit, with one ergonomic default: a single-statement
  program needs no `result` field. Multi-statement programs must name one
  (fail closed on ambiguity).
- Old/new both for updates: deferred. When change capture is really
  wanted, model pre/post images as explicit relations, not flags.

## LIR grows one pure node: the literal relation

▸ The ADR's "a literal row is simply a one-row relation" is currently
false — LIR has exactly eight node kinds (`scan`, `filter`, `project`,
`join`, `aggregate`, `order`, `slice`, `ref`) and `scan` is the only leaf.
There is no way to write a constant relation. Programs need one, and it is
a *pure, generally useful* addition (SELECT-from-VALUES queries, test
fixtures, seeding) rather than program-specific machinery:

```jsonc
{ "kind": "rows", "scope": "r",
  "columns": ["name", "age"],
  "rows": [["ada", 36], ["grace", 41]] }
```

Deterministic, never plan-choice-sensitive, cardinality = row count,
column types inferred per the literal-coercion rules the binder already
owns (raw JSON in, catalog-free wire converter, binder coerces — same as
`lit`). Touches the whole vertical (schema YAML → schemagen → lirwire →
validate → graphconv → bind → plan → execute → oracle interpreter), but
each piece is small and the corpus conventions make testing it mechanical.
Note the typeless-value footgun (tasks/1-todo/typeless-value-encoding.md)
applies to the row literals; encode with the explicit-null rule.

## What survives from revision one

- **Prove the isolation claim at the storage seam** — unchanged, and
  *more* urgent: programs put multi-statement read-your-writes at the
  centre of the contract. The test list stands: write-skew
  (two-doctors), phantom ranges, constraint races (duplicate unique
  values, FK parent deletion), backend conformance beyond in-memory
  SlateDB.
- **The error taxonomy split** — `invalid`/constraint violation (retry
  cannot help) vs `conflict`/serializable race (immediate retry may win) —
  now settled and enforced on the wire (commit `350058c`); programs adopt
  it wholesale. A failed statement rolls back the whole program and
  reports which statement failed and why.
- **Idempotency stays open.** Programs do not solve the lost-response
  problem: a client that never hears back from `/execute` cannot know
  whether the program committed. The client-supplied idempotency key with
  a bounded result record remains the likely mechanism, and matters more
  once one request carries a whole workflow.
- Immutable primary keys, composite-PK cell maps at the edges, and the
  documented generated-defaults semantics carry over as statement rules.

## Cutover blast radius (zero-legacy, one arc)

▸ Grounded inventory, so the estimate is honest:

- **Keep `protocol.Query` (nodes/bindings/root) intact as the statement
  payload.** The program envelope wraps it; the LIR document format,
  schema, validation, and graphconv do not change (beyond the `rows`
  node). This bounds the blast radius to the envelope.
- The battle-test corpus (~150 tests) and `tests/harness` speak
  `POST /query` via `radclient`; the harness `Result` seam absorbs the
  envelope change in one place. Harness `Insert` currently batches via
  `client.Txn` — it becomes a single program (or one `create` with a
  `rows` relation), which is strictly better and deletes its workaround
  comment.
- `client.Txn`/`Begin` are used by the generated Go client
  (codegen.go → tracker.go), the TS runtime, the harness, and
  `tests/e2e_test.go`. Generated clients re-emit their CRUD helpers as
  one-statement programs; their `Txn(fn)` becomes a program *builder*
  (statements accumulate, one `/execute` at the end) — same ergonomics,
  fewer round-trips, no session.
- `rad/server/api/sessions.go` and the `/tx` handler tree delete;
  dbserver_test's transaction cases become program cases (RYW inside one
  request instead of across requests).
- The devtool UI and keydecode are read-path; untouched. The catalog API
  (direct-catalog-mode) is control-plane; untouched.

Sequencing: land `rows` in LIR first (pure, independently testable), then
the program envelope + executor alongside the old endpoints, port harness
→ corpus → codegen → demo, then delete the five old endpoint families in
the same arc. `task demo` and `task demo:ts` remain the acceptance gates.

## Cross-ADR effects

- **error-propagation.md**: `Location` gains a `Statement` field; the
  stage list likely gains `program` (dependency/namespace validation
  failures are neither schema nor binding). The whole-program-rolled-back
  error must name the failing statement.
- **query-trace.md**: the trace artifact becomes program-scoped — one
  trace per program, statement spans at the top level, per-statement
  KV/planning sections under them. "An error is a trace that stopped
  early" now stops at a statement boundary.
- **relation bindings** (tasks/3-done/relation-bindings.md): the kernel
  extends, unmodified, to program scope; the only new rule is
  statement-result bindings are always materialised.

## Remaining open questions

1. **Does `/query` survive as a read-only shorthand?** Internally
   everything becomes a program either way. Leaning no (one endpoint, one
   model; the client library gives back the ergonomics), but the devtool
   and any curl-ability argument say maybe. Decide at build.
2. **Per-statement result summaries**: counts only, or keys? (Counts
   until a consumer demands more.)
3. **Program limits**: statement count, total input rows, result size —
   the execution-limits deferral (node/depth/payload) now has one more
   axis. Bound something before demos get creative.
4. **Update misses**: error vs skip-and-report. Proposed error above;
   revisit if import workflows want merge-ish tolerance (that's an upsert
   discussion, explicitly out of scope).
5. **Result selection shape**: string naming one statement (proposed) vs
   list of names → named results object. Start with one; a list is
   backwards-compatible later.

## Non-goals (carried and extended)

- Replacing HTTP/JSON; HTTP/2 + programs-as-batch is the early
  optimisation. Streaming/alternate codecs later, same semantics.
- Distributing live transactions; programs deliberately make requests
  stateless instead.
- Upsert, cascades, deferred constraints, expression-assignment UPDATE
  (`set x = x + 1` is expressible as a query feeding an update — that *is*
  the design), or a second condition language.
- Interactive transactions — reintroduce only with a workflow that
  genuinely needs to hold a snapshot across user think-time.

Related: tasks/3-done/relation-bindings.md (the reference machinery),
tasks/1-todo/error-propagation.md and query-trace.md (program-scoped
provenance), tasks/1-todo/typeless-value-encoding.md (row literals),
tasks/1-todo/enforced-ordering-in-certain-contexts.md (interacts with
mutation input determinism), rad/engine/06_frontend (Tx.Execute — the
read-your-writes seam that makes the executor a loop).
