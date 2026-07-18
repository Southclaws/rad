# Catalog revisions

Status: implemented — 2026-07-18.

Every committed catalog change produces a new schema version. The mechanism
is independent of migration source: a directly managed database has the same
revision history as a schema-managed database.

## First-iteration shape

- A fresh database is version zero. Its canonical schema is `{}`; it has no
  persisted revision record or timestamp because no change has committed.
- `/rad/catalog/meta/schema_version` stores the latest monotonic version.
- `/rad/catalog/meta/schema_revision/{version}` stores a JSON record containing
  the version, its UTC commit timestamp, and the complete canonical schema
  after that change. Version keys are zero-padded so a prefix scan returns
  history in version order.
- The catalog changes, current-version counter, revision record, and associated
  index backfills commit in the same serializable transaction. A failed change
  leaves all of them untouched.
- Revision records deliberately contain no mutation detail yet. Future history
  work can add the source channel, diff, generated data migration, and operator
  metadata without renumbering existing versions.

## Canonical schema

`catalog.Schema` is the durable logical schema shape. It reuses the catalog's
table, column, index, foreign-key, type, and default definitions. Stable,
human-authored table and column IDs are part of schema history; opaque physical
table/column/index/foreign-key IDs are not. Tables have deterministic identity
ordering, indexes and foreign keys have deterministic name ordering, and column
and key-column order is preserved because it is part of the declared shape.

Table IDs are positive integers unique across the database schema. Column IDs
are positive integers unique within their table, making a column identity the
pair `(table ID, column ID)`. IDs are immutable, have no ordering semantics,
and are never reused within a database's revision history. Direct catalog
operations allocate monotonically increasing IDs when callers omit them;
`schema.rad` requires authors to provide them. Renames retain the same ID and
may be combined with other structural edits in one migration, so the old
`renamed_from` hint is no longer part of the format.

The same shape is built from parsed `schema.rad` definitions and reconstructed
from physical catalog metadata. Revision creation always uses the latter: it
rebuilds the post-change state through the transaction's own view, then writes
that snapshot with the version counter in the same commit.

`Catalog.ValidateCurrentSchema` reads the latest stored snapshot and physical
catalog through one read snapshot and rejects `catalog_schema_drift` when they
differ. This gives tests and future startup validation a direct check that the
claimed schema and real catalog have not diverged.

## Increment semantics

- **Direct mode:** every successful individual catalog change increments the
  version once, including each step applied through the schema reconciler.
  Failed changes and successful no-ops do not increment it.
- **Schema mode:** one migration increments the version once, regardless of how
  many diff steps it contains. The diff and every step run in one transaction;
  a failed or empty migration does not increment the version.

The transaction-bound `catalog.Mutation` surface is the single implementation
point for catalog edits. Direct operations and reconciler steps each wrap one
mutation call, while a schema-managed reconciler groups the entire plan through
the same surface.

## Read surface

`GET /info` exposes `schema_version` and the optional `schema_version_at`
timestamp beside the catalog mode. The engine exposes the complete ordered
snapshot history through `Catalog.Revisions` for future history and diff APIs.

## Consumers

- Query traces can carry the schema version they were planned against.
- Generated clients can record the version they were generated from and detect
  drift through the cheap `/info` request.
- Future schema diffing, migration audit, adoption, and reconciliation tooling
  can extend the stored revision records.
