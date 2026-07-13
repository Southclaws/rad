# Unified error propagation and reporting

Status: refined in design review (2026-07-13) — the core model, taxonomy,
naming, and wire shape are settled below; remaining open questions are
listed at the end. Build later; the pre-build session is done.

The architecture in one line:

```text
Engine  →  structured error (E)  →  Problem builder  →  RFC 7807
```

Every engine layer only constructs an `E`; only the HTTP transport knows
RFC 7807 exists. This extends the layering `reject` already established —
the engine defines semantics, the transport defines representation — and
the guiding restraint is explicit: **the core stays tiny.** Postgres grew
SQLSTATE, DETAIL, HINT, and cursor positions organically over decades; we
are designing the same facilities intentionally, for a structured IR
instead of a string language, and the way to not regret it is to resist
the everything-bagel.

## Where we stand

- Classification is typed (`rad/engine/reject`: Input/Runtime markers),
  mapped to five problem codes; anything unmarked is a 500. Prefixes
  (`planner:`, `exec:`) are diagnostics only.
- Schema failures name node/binding + rule (the F1 best-match pass).
- Binder prose is good but unstructured — clients parse English.
- The wire `Problem` is one flat schema: `{type, title, status, detail,
  code}` with a code enum.

The gap: we classify and describe errors well but do not *locate* them or
*type their content*.

## The error value (settled shape)

```go
type E struct {
    Stage    Stage     // schema | preflight | binding | planning | execution | storage
    Class    Class     // invalid | execution_failed | conflict | not_found | internal
    Reason   Reason    // typed, stable, fine-grained
    Detail   string    // the human sentence — RFC 7807 terminology, so
                       // rendering is Problem.Detail = err.Detail
    Location *Location // where in the request document
    Meta     any       // one concrete per-class struct, never map[string]any
    err      error     // wrapped cause
}
```

Decisions folded in from review:

- **`Reason` is a typed string** (`type Reason string` + constants), not
  for the compiler but for autocomplete, typo-impossibility, exhaustive
  switches, documentation, and schema/codegen emission.
- **`Detail`, not `Msg`** — RFC 7807's own word; the Problem builder
  becomes mechanical.
- **`Meta` is never `map[string]any`.** Flexibility today is
  `meta["foo"]` archaeology in three years. Concrete structs per class
  (e.g. `RuntimeMeta{Operator, Table, Index string}`,
  `StorageMeta{...}`), carried as `any` but with a closed, documented set
  — which is exactly what lets OpenAPI document them (below).
- **`Location`, not `Source`** — "where the problem occurred" reads
  naturally; `location.pointer` doesn't overload "source". Its fields are
  orthogonal, not squeezed together:

  ```go
  type Location struct {
      Pointer string // RFC 6901 into the request document
      Node    string // LIR node id
      Binding string // binding name
      Scope   string // scope label
      Role    string // "predicate", "order term 0", "field 2"
  }
  ```

- **`Stage` is explicit, not inferred from reason.** `binding:
  unknown_column` vs `execution: division_by_zero` is immediately useful
  to a human debugging production, and it retires the string-prefix
  convention entirely — logs stop inventing prefixes because the stage is
  a field. (This also fixes the stacked-prefix wart `bindingErr` shipped:
  prefixes were in messages; now they're structure.)

## Taxonomy (settled)

Two levels: **class** (the five existing codes — frozen; they map to
status and to retryability) and **reason** (typed, grows forever). The
SQLSTATE lesson kept, its taxonomy not: classes almost never change,
reasons accumulate.

**Constraint violations split by retryability, not by SQL tradition.**
The client cares about exactly one thing — can retrying help?

```text
invalid / constraint_violation      → this request can never succeed
conflict / serializable_conflict    → the same request might succeed in 20ms
```

That is a cleaner and more useful split than SQLSTATE's historical
classes, and it is now the rule for classifying any future write-path
error.

**Type URIs stay class-level** (`urn:rad:problem:invalid`). Reasons are
enumerable members, not a second URI namespace.

Candidate reasons (harvested from errors the engine emits today):

- `invalid`: `schema_violation`, `unknown_table`, `unknown_column`,
  `unknown_scope`, `scope_closed`, `duplicate_scope`, `type_mismatch`,
  `literal_coercion`, `nondeterministic_first`, `scalar_arity`,
  `dependent_join`, `crossing_in_join`, `projection_collision`,
  `node_cycle`, `shared_node`, `unreachable_node`, `unknown_binding`,
  `unused_binding`, `binding_cycle`, `binding_output_collision`,
  `constraint_violation`.
- `execution_failed`: `division_by_zero`, `cardinality_violation`.
- `conflict`: `serializable_conflict`.
- `not_found`: `transaction`, later `table`/`row` for CRUD surfaces.

## The wire: a discriminated union of per-class Problems

The single flat `Problem` schema becomes a `oneOf` discriminated on
`code` — strictly typed per class, without encoding every individual
reason into the spec:

```yaml
Problem:
  oneOf:
    - $ref: "#/components/schemas/InvalidProblem"
    - $ref: "#/components/schemas/ExecutionFailedProblem"
    - $ref: "#/components/schemas/NotFoundProblem"
    - $ref: "#/components/schemas/ConflictProblem"
    - $ref: "#/components/schemas/InternalProblem"
  discriminator:
    propertyName: code
    mapping:
      invalid: "#/components/schemas/InvalidProblem"
      execution_failed: "#/components/schemas/ExecutionFailedProblem"
      not_found: "#/components/schemas/NotFoundProblem"
      conflict: "#/components/schemas/ConflictProblem"
      internal: "#/components/schemas/InternalProblem"
```

Each class carries its own extension shape — the typed replacement for
`map[string]any` at the wire level:

```text
Problem (base: type, title, status, detail, code)
├── InvalidProblem          reason, location?, errors[]?
├── ExecutionFailedProblem  reason, location?, execution?   (operator/table/index/binding)
├── ConflictProblem         reason, conflict?               (raced object, when resolvable)
├── NotFoundProblem         reason, resource?
└── InternalProblem         incident?                       (uuid; nothing else — ever)
```

The class shapes are stable spec surface; `reason` within each is a
string documented by a published registry (the typed constants), not an
OpenAPI enum per reason — categories in the spec, members in the
registry. Blast radius to note: ogen regenerates the Problem types as a
union (same pattern as the response unions), and `radclient.apiError` /
the TS runtime adjust once.

## Provenance (settled: option 1)

Unbound `lir` nodes gain an optional `ID string`, stamped by graphconv.
The justification that settled it: nodes already carry `Scope` — metadata
with no semantic weight. `ID` is the same kind of thing; engine-created
trees leave it empty and errors degrade gracefully to scope-level
location. This is shared machinery with the query-trace proposal
(tasks/1-todo/query-trace.md) — one provenance design, two consumers.

## Explain/trace convergence

Runtime errors point *into the operator graph*: an
`ExecutionFailedProblem.execution` carries the operator name in EXPLAIN's
vocabulary plus node/binding/index identity, so the admin UI highlights
the failing operator in red with no bespoke visualization. Error `E` and
the trace share Stage names, operator vocabulary, and Location — an error
is a trace that stopped early.

## Deliberately deferred

- **Multi-error accumulation** — deferred not because it's bad but
  because it infects the architecture: single errors keep every function
  `(..., error)`; accumulators spread everywhere. The wire shape is
  designed now (`errors[]?` on InvalidProblem, each element a full
  sub-problem) so it can land without a contract break — built only when
  a real IDE-shaped consumer needs it.
- **500 content**: `incident` uuid at most. Nothing else, ever.

## The work, when scheduled

1. `E`, `Stage`, `Class`, typed `Reason`, `Location`, per-class meta
   structs — grow `reject` or a sibling leaf package.
2. OpenAPI: the discriminated Problem union + regen; client surfaces
   (`Reason()`, `Location()`, class predicates in Go; same fields on the
   TS error).
3. Provenance: `ID` on unbound nodes, stamped in graphconv.
4. Sweep reasons + stages onto existing sites (third pass over the F3
   sites); pointer locations onto schema/preflight errors (nearly free —
   the validator has instance locations); node locations onto binder
   errors.
5. Execution meta on runtime/conflict paths; conflict-object reverse
   lookup (key prefix → catalog identity).
6. Harness `ExpectReason`/`ExpectLocation`; convert the corpus's ~30
   message-substring probes to reason assertions — contract decoupled
   from prose.

Redaction rule (standing, shared with query-trace): schema names — tables,
indexes, columns, bindings, node ids — always; key bytes and row values
never.

## Remaining open questions (small)

- Exact per-class meta shapes: what belongs in `execution` vs the trace
  (lean minimal here — identity, not metrics; metrics live in the trace).
- Does `not_found.resource` earn its structure now or when CRUD surfaces
  grow?
- `Stage` granularity at the bottom: is `storage` a stage of its own or
  folded into `execution`? (Lean: separate — a KV failure is not a query
  semantics failure.)
- Where the Reason registry lives so docs, Go constants, and the TS
  client stay in sync (one generated source, probably alongside the
  schema pipeline).

Related: tasks/1-todo/query-trace.md (shared provenance, stage names,
operator vocabulary); tasks/1-todo/lir-query-validation.md and
tasks/1-todo/validation-and-sharing-semantics.md (validation-side reasons
reconcile with this taxonomy); tasks/3-done/lir-improvements.md (F1/F3 —
the prior art this extends).
