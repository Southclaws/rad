# Direct catalog mode and imperative catalog operations

Status: scoped (2026-07-13) — user-authored ADR reviewed, grounded, and
the three review findings resolved. This phase lays the foundation:
catalog modes + the imperative (REST-ish) catalog mutation API. Admin UI
editors build on it next; catalog revisions are split out
([catalog-revisions](catalog-revisions.md)).

## The model

Two catalog management modes per database, **set once at boot, never
changed** (for now — adoption Direct→Schema is future work and must not
be architecturally precluded):

- **Direct** (default — easy demo/playground) — the database is the
  source of truth. Imperative catalog mutation endpoints and the Admin UI
  editors work. `rad serve` → browser → build a schema with no files.
- **Schema** — `schema.rad` is the source of truth. Imperative catalog
  endpoints are rejected before any change occurs; `SchemaMigrate` is the
  only mutation path; the Admin UI is inspection/debugging only.

`SchemaMigrate` stays available in **both** modes — in Direct mode it is
just another way to mutate. Read-only catalog APIs stay open in both
modes. Codegen works in both: `rad schema pull` (future, with the
catalog→schema.rad renderer) reads the live catalog for `rad generate`.

Non-goals (from the ADR, endorsed): no drift reconciliation, no mixed
ownership, no auto-merge, no partial management, no Terraform-style
state.

## Settled decisions

1. **Imperative REST-ish endpoints, not a reconciler wrapper.** The
   direct channel is fine-grained: create table, modify table, delete
   table, plus column CRUD (and index create/drop — the ADR's "manage
   indexes"). These map onto the same engine operations
   (02_catalog/mutations.go) the reconciler drives, so capability parity is a
   rule: anything the reconciler rejects (e.g. column type changes) the
   imperative API rejects identically — one capability surface, two
   grammars over it.
2. **Mode is stored in the catalog** — one KV entry, written when fresh
   storage is initialised, read inside the same transaction as each
   mutation check (snapshot-consistent, restart-proof, cheap).
3. **Set at boot only**: `rad serve` flag/env (e.g. `RAD_CATALOG_MODE`)
   applies **only when initialising a fresh database**; absent flag ⇒
   Direct. On an existing database the persisted mode wins; an explicitly
   mismatched flag is a startup error (loud beats silent — mode is
   set-once, so a mismatch is operator confusion).
4. **Error shape: don't sweat it yet.** Rejections in Schema mode use the
   existing `reject` input-classification (422 invalid today is fine);
   the settled taxonomy work (tasks/1-todo/error-propagation.md) sweeps
   this site later — noted there as the first test of the retryability
   rule (schema-managed rejection is never retryable, so it is *not*
   `conflict` despite the ADR's 409 sketch).
5. **Catalog revisions deferred** → [catalog-revisions](catalog-revisions.md).
   Nothing in this phase depends on them.

## Grounding (what exists, 2026-07-13)

- Wire catalog mutation today = `SchemaMigrate` only
  (rad/server/api/dbserver.go); the fine-grained surface
  (`Catalog.CreateTable`, mutations.go) is engine-internal, reachable
  only through the reconciler. The imperative API exposes it.
- Admin UI (rad/ui: TableView, KVBrowser; served on :7238) is
  inspection-only — Direct-mode editors are the additive half, built
  after this phase.
- `rad generate` reads `-f schema.rad`; `rad schema pull` needs the
  catalog→schema.rad renderer (inverse of the 02_catalog/schema parser)
  — future work, three consumers (pull, UI schema view, adoption). A
  pull is a snapshot: no `renamed_from` history.

## The work

1. Mode storage in 02_catalog: KV entry + accessor, stamped on fresh
   init, `rad serve` bootstrap flag/env, mismatch = startup error.
2. Engine: ensure the catalog exposes the imperative set the API needs
   (create/drop table, add/drop/rename column, add/drop index) as
   transactional operations with the same validation the reconciler uses.
3. Wire: OpenAPI endpoints for table/column/index CRUD, mode-gated;
   regen; client surfaces as needed by the UI.
4. Expose mode (and later revision) on a read endpoint so the UI can
   grey out mutation affordances in Schema mode with the ADR's banner.
5. Tests: mode gating matrix (imperative × migrate × both modes),
   set-once semantics (fresh init honours flag; existing DB ignores
   absent flag, errors on mismatch), imperative-vs-reconciler capability
   parity, schema changes through the wire end-to-end.

Then (follow-on phases): Admin UI editors + data import over the
imperative API; catalog revisions; `rad schema pull` + renderer;
adoption workflow.

Related: tasks/1-todo/catalog-revisions.md,
tasks/1-todo/error-propagation.md (rejection taxonomy sweep),
tasks/1-todo/query-trace.md (UI consumer),
tasks/1-todo/schema-flexibility.md (whatever loosens the schema loosens
both channels equally).
