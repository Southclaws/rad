# Unified error propagation and reporting

Status: proposal — ideas to refine together pre-build. The goal is one
error mechanism from any step of the engine — schema validation, preflight,
binding, planning, execution, storage — landing in a well-structured RFC
7807 Problem JSON with semantic codes, positional information into the LIR
document, and (where it helps) physical execution context. Nothing here is
settled; the sketches below are starting positions.

## Where we stand (post battle-test campaign)

Worth restating because it is the foundation this builds on, not a green
field:

- **Classification is typed, not textual** (F3): `rad/engine/reject` marks
  errors `Input` (caller's fault at request time) or `Runtime`
  (data-dependent failure of a valid query); the server maps types to
  problem codes — `invalid`, `execution_failed`, `conflict`, `not_found`,
  `internal` — and anything unmarked is a 500. The prefix strings
  (`planner:`, `exec:`) are diagnostics only.
- **Schema failures name the problem** (F1): the kind-directed best-match
  pass reports `node "s" (slice): …rule…` and `binding "x": …` instead of
  oneOf dumps.
- **Binder messages are good prose**: named scopes/columns/tables, the
  scope-closed diagnosis, remedy hints (cast, name-it-as-a-binding). But
  they are *prose* — a client cannot machine-read where the error is.
- **The wire shape is flat**: `{type, title, status, detail, code}`. All
  structure beyond `code` lives inside the `detail` string.

The gap, in one sentence: we classify errors well and describe them well,
but we do not *locate* them or *type their content* — a query compiler, a
UI, or a retry loop has to parse English.

## Goals

1. Any engine step can raise a structured error that survives to the wire
   without loss: class, fine-grained reason, human message, source
   location, and step-appropriate context.
2. The Problem JSON carries that structure in extension members — RFC 7807
   explicitly invites this — while `detail` stays a readable sentence.
3. Codes are a stable, testable contract (the harness already has
   `ExpectCode`; it grows `ExpectReason` / `ExpectSource`).
4. Internal errors stay internal: structure must never widen what a 500
   reveals.

## Non-goals (for this ADR)

- Retry policy / client backoff (belongs to the client runtime).
- Observability plumbing (logs/traces) — same error values will feed it,
  but transport is out of scope here.
- Warnings/notices (non-fatal diagnostics) — possibly a `warnings: []`
  response extension one day; noted, not designed.

## Sketch 1 — one structured error value

Grow `reject` (or a sibling leaf package) from binary markers into the
engine-wide error value. Something like:

```go
type E struct {
    Class  Class          // invalid | execution_failed | conflict | not_found | internal
    Reason string         // fine-grained, stable: "unknown_column", "division_by_zero"
    Msg    string         // the human sentence (today's detail)
    Source *Source        // where in the request document (sketch 3)
    Meta   map[string]any // step-appropriate context (sketch 4)
    err    error          // wrapped cause
}
```

Constraints from today's doctrine: the package imports nothing (layer-0
leaf, like `reject`); `errors.As`-friendly; the prose message remains
derivable so logs stay readable without the structure. The `planner:` /
`exec:` prefixes stop being embedded in strings and become a rendered
property of Class — which also fixes a wart we shipped this week:
`bindingErr` wrapping currently produces `planner: binding "x": planner:
unknown scope …` (stacked prefixes), because prefixes live inside messages.
Structure kills that class of bug.

Migration is a second sweep over the sites the F3 sweep already visited —
each `reject.Inputf("planner: …")` gains a reason and (where known) a
source. Mechanical, reviewable, and the corpus's `ExpectError` assertions
keep messages honest throughout.

## Sketch 2 — the code taxonomy

Two levels, not one flat namespace: **class** (the five existing codes,
unchanged — they map to status and to retry-ability) and **reason** (new,
fine-grained, stable). Wire shape:

```json
{
  "type": "urn:rad:problem:invalid",
  "title": "Invalid Request",
  "status": 422,
  "detail": "planner: scope \"t\" exists but is not visible here — …",
  "code": "invalid",
  "reason": "scope_closed",
  "source": { "node": "out", "pointer": "/nodes/out/terms/0/expr" }
}
```

Candidate reasons, harvested from the errors we actually emit today:

- `invalid`: `schema_violation`, `unknown_table`, `unknown_column`,
  `unknown_scope`, `scope_closed`, `duplicate_scope`, `type_mismatch`,
  `literal_coercion`, `nondeterministic_first`, `scalar_arity`,
  `dependent_join`, `crossing_in_join`, `projection_collision`,
  `node_cycle`, `shared_node`, `unreachable_node`, `unknown_binding`,
  `unused_binding`, `binding_cycle`, `binding_output_collision`,
  `constraint_violation` (writes: duplicate PK, FK, unique, not-null).
- `execution_failed`: `division_by_zero`, `cardinality_violation` (the
  exactly_one miss), later cast overflow etc.
- `conflict`: single reason today (`serializable_conflict`), room for
  `unique_race` vs `write_write` if the KV layer can distinguish.

Precedents worth stealing from, deliberately: Postgres SQLSTATE's
class/subclass split (coarse code stays stable while fine codes grow),
Google's `reason` + `domain` + structured detail types, Stripe's
`code` + `param`. The five-class ceiling is a feature — reasons may grow
freely, classes should essentially never.

Open question: does `type` stay class-level
(`urn:rad:problem:invalid`) or gain the reason
(`urn:rad:problem:invalid/scope-closed`)? Class-level keeps type URIs
enumerable; the `reason` member carries the rest. I lean class-level.

## Sketch 3 — source locations (the provenance problem)

Three positional vocabularies, used where each is natural:

- **JSON Pointer** (RFC 6901) into the request document —
  `/nodes/open_b1/predicate/left`, `/bindings/x/node`. The schema
  validation layer has instance locations essentially for free (the
  validator reports them; we currently flatten to prose). Highest value
  per effort: 400s get `source.pointer` almost immediately.
- **Node id + role** — `{node: "out", role: "order term 0"}` — the
  granularity binder errors naturally have.
- **Binding name** — already prefixed in messages; becomes
  `source.binding`.

The real design problem: **node ids are erased before the binder runs.**
`graphconv` materialises the wire's flat map into unbound value structs;
by bind time an error knows the scope name but not the node id or JSON
path. Options to weigh pre-build:

1. **Provenance field on unbound nodes** — each `lir.*` relation struct
   gains an optional `ID string` stamped by graphconv (engine-direct
   callers leave it empty; errors degrade gracefully to scope-level
   location). Cheap, slightly pollutes the IR.
2. **Provenance side-channel** — graphconv returns a positions structure
   the binder threads alongside. Keeps the IR clean; value structs can't
   be map keys, so this needs paths mirrored structurally (fiddly).
3. **Bind against the wire form directly** — dissolve graphconv into the
   binder so positions are never lost. Biggest change; also removes a
   layer of translation. Probably too much for v1 but worth saying out
   loud.

I lean (1): optionality matches how `Scope` already behaves, and the ADR
principle that ids are labels means carrying the label is harmless.
Physical-plan provenance follows the same thread: `AttachSpec`/`RefExec`
already carry enough (slot, binding name) to say *which* crossing or
occurrence failed at runtime.

## Sketch 4 — execution context for runtime and conflict errors

For `execution_failed` and `conflict`, positional info matters less than
*execution* info. Candidates for `meta`:

- The failing physical operator: table and index names
  (`{op: "IndexRangeScan", table: "orders", index: "orders_cust_status_idx"}`),
  the binding name for a commitment failure, the crossing kind for an
  attach failure.
- For conflicts: which logical object the tracked range belonged to
  (table/index name), so a retry loop can report *what* raced. The KV
  layer knows the range; mapping range → catalog object is a small
  reverse lookup (key prefixes carry table/index ids).
- Op counts at failure (`scans`, `gets` so far) — nearly free given the
  counting infrastructure the tests already use, and a gift for support.

**Redaction rule to settle up front**: key bytes and row values never
appear in problem output — they are user data. Names of tables, indexes,
columns, bindings, and node ids are schema-level and fine. (Today's
messages already follow this instinctively — e.g. the unique-violation
message names the index, not the value; make it a stated rule.)

Cross-link: this is the same seam as the planned explain decoration —
explain describes the plan before/while it runs; error meta describes
where it stopped. They should share vocabulary (operator names as printed
by EXPLAIN) and probably share the response-envelope work.

## Sketch 5 — multi-error reporting

The binder fails fast; a compiler or UI wants everything wrong at once,
like a type-checker. RFC 7807 handles it with an extension member:

```json
{ "code": "invalid", "detail": "3 problems found", "errors": [ {…}, {…}, {…} ] }
```

where each element is a full sub-problem (reason/source/detail). Cost: an
error accumulator through the binder walk — real restructuring, since slot
assignment continues past failures only if binding can proceed on a
placeholder. Suggest phase 2, but design the wire shape now so single
errors are just the one-element degenerate case (or `errors` is only
present when n > 1).

## Client surface

- Go: `APIError` grows typed accessors (`Reason()`, `Source()`, class
  predicates); `IsConflict` stays.
- TS runtime: the same fields on the thrown error object.
- Harness: `ExpectReason("scope_closed")`,
  `ExpectSource("/nodes/out/...")` — and the existing corpus's ~30 error
  probes convert from message-substring assertions to reason assertions,
  which finally decouples the *contract* (codes) from the *prose*
  (improvable without breaking tests).

## Suggested shape of the work (pre-build refinement, then)

1. Settle the `E` type, class/reason split, and the redaction rule.
2. Wire shape: `reason`, `source`, `meta`, `errors` extension members in
   the Problem schema (OpenAPI change, not lir.schema.yaml).
3. Provenance decision (sketch 3) — the only part touching the IR.
4. Sweep: reasons onto existing sites; pointer sources onto schema/preflight
   errors (cheap); node sources onto binder errors (needs 3).
5. Execution meta for runtime/conflict paths.
6. Harness + corpus conversion to reason-based assertions.
7. Phase 2: multi-error accumulation; conflict-object resolution.

## Open questions for the refinement session

- Class-level vs reason-level `type` URIs.
- Provenance option (1) vs (2) — or defer node-level sources and ship
  pointer sources for schema errors only?
- Do write-path constraint violations stay `invalid` or earn their own
  class (SQLSTATE separates integrity violations from syntax)? The code
  is load-bearing for clients (`conflict` means retry; should
  `constraint_violation` mean "fix your data"?).
- Is `errors[]` worth the binder restructuring, and if so which errors
  can safely accumulate (name resolution yes; type inference after a
  failed resolution, probably not)?
- How much meta on 500s? (Lean: none — an opaque incident id at most.)

Related: tasks/1-todo/lir-query-validation.md and
tasks/1-todo/validation-and-sharing-semantics.md overlap on the
validation-side reasons and should be reconciled with this taxonomy;
tasks/3-done/lir-improvements.md (F1/F3) is the prior art this extends.
