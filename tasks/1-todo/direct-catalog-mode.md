# Direct catalog mode and admin UI foundation

Status: proposed (2026-07-13) — user-authored ADR, reviewed and grounded
against the codebase. Direction accepted; the grounding notes below adjust
the mechanism, not the goals.

## The proposal (summary)

Two explicit catalog management modes per database:

- **Direct** — the database is the source of truth. Create/modify/drop
  tables, indexes, import data, all through the Admin UI or HTTP API. The
  playground mode: `rad serve` → browser → build a schema with no files.
- **Schema** — `schema.rad` is the source of truth. Direct catalog
  modification is rejected server-side before any change occurs; the Admin
  UI becomes inspection/debugging only.

Every catalog mutation checks the mode first. Codegen works in both modes:
`rad schema pull` reads the live catalog into a local schema representation
that `rad generate` consumes. Adoption path Direct → pull → commit
schema.rad → Schema mode; the adoption *mechanism* is out of scope but must
not be architecturally precluded.

Non-goals (explicit, endorsed): no drift reconciliation, no mixed
ownership, no auto-merge, no partial management, no Terraform-style state.

Success shape: download → `rad serve` → Admin UI (:7238) → create schema →
insert data → query → inspect plans/traces → generate a typed client.

## Grounding against what exists (2026-07-13)

- **There are no fine-grained catalog mutation endpoints to gate.** The
  wire's only catalog mutation today is `SchemaMigrate`
  (rad/api/openapi.yaml → rad/server/api/dbserver.go): reconcile the
  database with a `schema.rad` document — declarative, idempotent,
  diff-based, rename hints via `renamed_from`. The fine-grained surface
  (`Catalog.CreateTable`, ddl.go) is engine-internal, reachable only
  through the reconciler.
- **Therefore the central design question the ADR glosses: migration IS a
  catalog mutation, through the same pathway.** "Schema mode rejects
  catalog mutations" cannot mean "reject the mutation endpoints as a set"
  — SchemaMigrate must stay open in Schema mode; it is how Schema mode
  changes anything. The mode gates a *direct channel*, not mutations.
- **Catalog revisions do not exist yet.** The ADR says mutations should
  "continue to produce a catalog revision" — this is new work, not
  preservation. There is already a second consumer queued: the query-trace
  proposal's `InputTrace` carries the catalog revision
  (tasks/1-todo/query-trace.md).
- **`rad schema pull` needs the inverse renderer.** cmd/rad has serve,
  validate, migrate, generate; generate reads `-f schema.rad`. The
  parser direction (schema source → catalog defs) exists in
  02_catalog/{schema,migrate}; catalog → schema.rad source does not.
- **The Admin UI is inspection-only today** (TableView, KVBrowser; wire
  :7237, UI :7238) — consistent with the ADR's Schema-mode UI, so Direct
  mode is the additive half.

## Mechanism refinements (from review)

1. **The direct channel is a second endpoint sharing the reconciler.**
   Cheapest correct v1: a mode-gated `CatalogApply` (structured table/index
   defs as JSON — a UI speaks structs, not schema.rad text) that feeds the
   same diff/apply engine SchemaMigrate uses. One mutation engine, two
   doors; validation, rename handling, and idempotence for free.
   Fine-grained REST-ish DDL endpoints (TableCreate, IndexDrop, …) are a
   UX decision for later — nothing here precludes them, and they'd gate on
   the same mode check.
2. **SchemaMigrate stays available in both modes.** In Direct mode it is
   just another way to mutate (and the natural adoption on-ramp — a
   successful migrate against a Direct database is where an explicit
   mode-flip step would slot in later). Gate check therefore lives on the
   direct channel only.
3. **Mode is stored in the catalog itself**, one KV entry read inside the
   same transaction as the mutation — snapshot-consistent, restart-proof,
   and "inexpensive to determine" as the ADR requires. New databases
   default to Direct (the playground goal). A `rad serve` flag/env may set
   the initial mode on an *empty* database only — no config-vs-stored
   disagreement.
4. **Rejection shape must follow the error ADR, and the ADR's own sketch
   doesn't.** `urn:rad:problem:catalog-schema-managed` violates the
   settled rule that type URIs stay class-level; and 409/`conflict` is
   settled as *retryable* ("the same request might succeed in 20ms"),
   which this is not — no retry succeeds until an operator changes the
   mode. By the retryability rule this is `invalid` (422), reason
   `schema_managed`, detail as proposed ("Direct catalog modifications are
   disabled because this database is managed by schema migrations."). This
   is the first real test of the retryability split; note it in
   tasks/1-todo/error-propagation.md when built.
5. **Catalog revisions**: monotonic counter bumped inside every ddl
   transaction, independent of migration history (as the ADR requires).
   Serves auditing/diffing later, the trace artifact now, and client
   compatibility checks eventually. Keep v1 to the counter (+ timestamp);
   the mutation log is future work.
6. **The catalog → schema.rad renderer is one artifact, three consumers**:
   `rad schema pull`, the Admin UI's "show me this database as a schema
   document" view, and the adoption workflow. `renamed_from` hints are
   inherently absent from a pull — it is a snapshot, not a history; say so
   in the command's help.
7. **The UI learns the mode from a read endpoint** (health or a catalog
   metadata response) and greys out mutation affordances in Schema mode
   with the ADR's banner text. Read-only catalog APIs stay open in both
   modes.

## The work, when scheduled

1. Mode storage in 02_catalog (KV entry, default Direct, bootstrap-only
   serve override) + catalog revision counter in the ddl txn.
2. Wire: `CatalogApply` (structured defs → shared reconciler) gated by
   mode; mode + revision exposed on a read endpoint; rejection as
   `invalid`/`schema_managed` per the error taxonomy.
3. Catalog → schema.rad renderer; `rad schema pull` command.
4. Admin UI direct-mode surfaces: table/column/index editors, data
   import/insert, driven by CatalogApply; mode banner in Schema mode.
5. Tests: mode gating (both channels × both modes), revision monotonicity,
   pull→generate round-trip (pulled schema migrates to an identical
   catalog — the reconciler's idempotence makes this a one-line
   assertion), UI api surface.

Future work (unchanged from the ADR): schema explorer, planner/trace
visualisers, storage browser, import formats, adoption workflow — none
require changes to the mode model.

Related: tasks/1-todo/query-trace.md (UI consumer; catalog revision in
InputTrace), tasks/1-todo/error-propagation.md (rejection taxonomy),
tasks/1-todo/schema-flexibility.md (whatever loosens the schema loosens
both channels equally).
