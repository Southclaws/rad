# Catalog revisions

Status: todo — split out of [direct-catalog-mode](direct-catalog-mode.md)
(2026-07-13). Not needed for the catalog-modes phase; do after it.

Every catalog mutation — imperative or via the `SchemaMigrate`
reconciler — produces a new catalog revision. The revision mechanism is
independent of migration history (a Direct-mode database with no
migrations still has revisions).

## Shape (v1: keep it to the counter)

- Monotonic counter, bumped inside the same ddl transaction as the
  mutation — never drifts from the catalog state it describes.
- Timestamp alongside it, nothing more. A mutation log (what changed,
  by which channel) is future work; design the counter so a log can hang
  off it later without renumbering.
- Exposed on a read endpoint (same surface that reports the catalog
  mode) so clients and the UI can cheaply detect "the schema changed
  under me".

## Consumers (why it earns its place)

- **Query trace**: `InputTrace` carries the catalog revision the query
  was planned against (tasks/1-todo/query-trace.md).
- **Client compatibility**: a generated client can record the revision
  it was generated from and detect drift.
- **Future**: auditing, schema diffing, the schema-adoption workflow,
  reconciliation tooling (all from the original direct-catalog-mode ADR's
  future-work list).

Related: tasks/1-todo/direct-catalog-mode.md (the phase this supports),
tasks/1-todo/query-trace.md (first concrete consumer).
