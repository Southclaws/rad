# Direct catalog mode and imperative catalog operations

Status: DONE (2026-07-13) — commits `ea01c6d` (engine) and `0adf588`
(wire). This phase delivered catalog modes and the imperative catalog API;
the Admin UI editors build on it next.

## What shipped

**Modes.** Every database is `direct` (default — the live catalog is the
source of truth) or `schema` (schema.rad migrations own the catalog). The
mode is catalog metadata (`/rad/catalog/meta/mode`), stamped once at first
initialisation and immutable: `rad serve --catalog-mode` / `RAD_CATALOG_MODE`
applies only to a fresh database, and an explicit mismatch with the stored
mode is a startup error. `/health` reports the mode; the server reads it
once at construction (immutability makes the cache sound), so the gate on
the imperative endpoints is free.

**One mutation surface.** The engine's catalog operations
(`CreateTable`, `DeleteTable`, `RenameTable`, `CreateColumn`,
`DeleteColumn`, `RenameColumn`, `CreateIndex`+backfill, `DeleteIndex`) are
fronted by a single frontend façade (06_frontend/catalog.go) that both the
schema.rad reconciler and the wire endpoints drive — capability parity by
construction. Enforcement lives only at the transport: a schema-managed
database rejects each imperative endpoint with an `invalid` problem
("schema-managed" detail) before touching anything.

**RESTful wire surface** (plain CRUD vocabulary — no DDL, no drop/alter):

- `POST /tables` — create a whole table in one call: columns (with
  generator/literal defaults), primary key, indexes, foreign keys.
- `PATCH /tables/{table}` — update properties (today: `name`).
- `DELETE /tables/{table}`.
- `POST /tables/{table}/columns`, `PATCH|DELETE
  /tables/{table}/columns/{column}`.
- `POST /tables/{table}/indexes`, `DELETE
  /tables/{table}/indexes/{index}` — create backfills atomically; a unique
  index over duplicate data rejects as `invalid` and registers nothing.

Names in paths are unvalidated for now (URL-encoding carries UTF-8);
name rules are a separate future todo. `TableInfo` introspection now
returns indexes, foreign keys (referenced table by *name*), and column
defaults; mutations return the updated table. Go client
(`rad/client/catalog.go`) has the matching typed methods plus `Mode`.

**Engine hardening found by this work.** `DeleteTable` now refuses while
another table references the target through a foreign key (self-references
excepted) — previously only the migration differ checked, so direct
deletes could strand dangling `RefTableID`s — and the differ orders
multi-table deletes referencing-table-first to match.

**Terminology.** "DDL" is gone repo-wide (it's an API, not a language),
and SQLy verbs (drop/add/alter) are replaced by create/update/delete in
code, wire, errors, step strings, tests, and docs.

## Tests

- 02_catalog: mode default/stamp/set-once/mismatch; delete guards incl.
  self-reference; existing evolution suite renamed to mutations_test.
- migrate: delete ordering with adversarial names.
- rad/server/api/catalog_test.go (wire, radclient over httptest): whole
  table in one call incl. defaults round-trip; validation matrix with
  atomicity probe; full lifecycle (rename table/column with live data,
  index create on populated table, guards, delete-to-empty); unique
  backfill rollback + retry + constraint liveness; referential guards;
  schema-mode gate across all eight ops with migrate still open; direct
  mode accepts both channels; unknown targets are 422s not 500s.

## Follow-on phases (not this task)

- Admin UI editors + data import over these endpoints; mode banner
  (UI reads mode from the admin plane or /health).
- [catalog-revisions](../1-todo/catalog-revisions.md).
- `rad schema pull` + catalog→schema.rad renderer.
- Name validation rules (URL-safety, reserved names) — own todo.
- Adoption workflow (direct → schema) — mode stays set-once until then.

Docs: home/content/docs/engine/02-catalog.mdx gained a "Catalog management
mode" section; method names synced with the CRUD rename.
