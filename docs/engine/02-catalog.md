# Layer 02: the catalog

Package `catalog` (directory `rad/02_catalog`) owns the schema: table
definitions with their columns, primary keys, secondary indexes, and foreign
keys. It is the second layer of the engine and imports only layer 01 (`kv`).
Everything above it — the LIR's type vocabulary (03), the binder (04), the
executor (05), and the frontend (06) — reads schema through this package and
never touches catalog keys directly.

The catalog is plain data in the KV store. A `Catalog` value holds nothing but
a `kv.TransactionalKV` handle (`catalog.New(store)`); there is no in-memory
schema cache, no version counter, no watch mechanism. Two `Catalog` values
over the same store are interchangeable, and `TestPersistenceAcrossInstances`
pins that: state lives entirely in the keyspace. A Rad instance is exactly one
database — there is no schema or database hierarchy; two databases are two
Rad deployments.

## Keyspace

The catalog occupies three key families, all plain UTF-8 strings (not
`keyenc` tuples):

```
/rad/catalog/meta/next_id        -> decimal counter, e.g. "17"
/rad/catalog/table/<id>          -> JSON-encoded Table, e.g. .../table/t1
/rad/catalog/table_name/<name>   -> table ID, e.g. .../table_name/users -> t1
```

`Table` and its nested `Column`, `Index`, and `ForeignKey` structs are the
stored representation and the API representation at once — they marshal
directly to the JSON value under `/rad/catalog/table/<id>`. Row data and
index entries live elsewhere (under keys built by 05_exec from table and
index IDs); the catalog stores metadata only.

## Identity: IDs are stable, names are labels

Every table, column, index, and foreign key carries two identifiers: a
human-facing `Name` and a catalog-assigned `ID`. IDs are strings of a kind
prefix plus a number — `t3` for tables, `c17` for columns, `i5` for indexes,
`fk2` for foreign keys — all drawn from the single shared counter at
`/rad/catalog/meta/next_id` by `nextID`. Because the counter is shared, IDs
are unique across kinds, not merely within one (the tests assert no ID
collides across a table, its columns, and its indexes). IDs are never reused,
including after drops.

Callers define schema in terms of names (`TableDef`, `ColumnDef`, `IndexDef`,
`ForeignKeyDef` carry no IDs); the catalog assigns IDs at creation time.
Data keys use IDs, never names: rows are keyed by table ID, row values by
column ID, index entries by table and index ID. Two properties follow:

- **Renames are metadata-only.** `RenameTable` rewrites one name-index key
  and the table blob; `RenameColumn` rewrites the column's name and every
  metadata reference to it (primary key, index column lists, FK column lists)
  inside the blob. No row or index entry is touched, and the ID — and
  therefore the data — is preserved (`TestRenameColumnRewritesReferences`,
  `TestRenameTableKeepsIdentity`).
- **Drops orphan data safely.** `DropTable` and `DropIndex` delete metadata
  only; the abandoned rows and index entries are unreachable garbage awaiting
  a future vacuum (a documented POC trade-off). Since IDs are never reused, a
  later table can never accidentally adopt an orphan's keys.

Foreign keys store `RefTableID`, not the referenced table's name, so renaming
a parent table does not invalidate its children's FKs.

`nextID` is read-modify-write on one key. It is safe only because every
caller runs it inside a `SerializableSnapshot` transaction: two concurrent
allocations both write the counter key, so one commit fails with
`kv.ErrConflict` instead of both minting the same ID.

## The read API

Three methods resolve schema, all returning fully materialized `Table`
values (the JSON blob decoded whole — columns, PK, indexes, and FKs
included):

- `GetTable(ctx, name) (Table, bool, error)` — two point reads: name key to
  ID, then ID to metadata. The `bool` is false for an unknown table.
- `GetTableByID(ctx, id) (Table, bool, error)` — one point read.
- `ListTables(ctx) ([]Table, error)` — a scan over `/rad/catalog/table/`,
  sorted by name.

On the returned `Table`, `Column(name)` and `Index(name)` resolve members by
name with linear scans. There is no caching at any level: every `GetTable`
hits the store and unmarshals JSON. The binder (`planner.Bind` in
`rad/04_planner/bind.go`) calls `GetTable` once per `Scan` node it binds and
carries the resulting `Table` through the rest of the walk; the executor's
`Engine.table` does the same per write request.

**Which KV view readers use matters.** The read methods go through `c.store`
directly — the `TransactionalKV`'s plain `KV` surface — so each `Get` and
`Scan` is an independent, autocommit read of the *current committed state*.
Readers neither take a snapshot per call nor hold one across calls. Two
consequences downstream code must not forget:

- Catalog reads are **not** part of any enclosing data transaction. Even
  `Tx.Execute` (05_exec) binds via `planner.Bind(ctx, e.cat, q)`, which reads
  schema from committed state, not from the transaction's snapshot, and adds
  nothing to its read set. A concurrent DDL commit is invisible to the
  transaction's conflict detection; the design leans on DDL being rare and on
  IDs being stable (a stale `Table` still points at real data keys).
- Within one `GetTable`, the name read and the metadata read are two separate
  committed-state reads with no snapshot spanning them. A drop or rename
  between the two resolves as "not found" or as the pre-rename blob — never
  as another table's data, again because IDs are never reused.

The only read paths that *are* transactional are the unexported
`getTable`/`getTableByID` helpers, which take an explicit `kv.KV` view — DDL
uses them internally, and `AddIndexIn` exposes that capability for index
creation (below). There is no exported transaction-scoped `GetTable`.

## DDL

All DDL is transactional. The `ddl` helper begins a `SerializableSnapshot`
transaction, runs the mutation against the transaction's view, and commits;
any error rolls the whole operation back. `TestCreateTableValidation` pins
the atomicity: a rejected `CreateTable` leaves nothing behind — not the name
key, not the counter bump — so the name can be retried immediately.

`CreateTable(ctx, def TableDef) (Table, error)` validates the definition,
allocates IDs for the table and every column, index, and FK, resolves FK
`RefTable` names to IDs (self-references resolve against the table being
created), and writes both the table blob and the name-index key in one
commit.

Schema evolution lives in `ddl.go`, each operation in its own transaction
built on `mutateTableIn` (load blob, apply closure, save blob):

- `RenameTable(ctx, old, new)`, `RenameColumn(ctx, table, old, new)` —
  metadata-only, as above.
- `AddColumn(ctx, table, def)` — appends a column with a fresh `c<N>` ID.
- `DropColumn(ctx, table, col)` — removes the column after reference checks.
- `AddIndex(ctx, table, def) (Index, error)` — registers an index; suitable
  only for tables with no rows (see the backfill contract below).
- `DropIndex(ctx, table, index)` — removes the index metadata.
- `DropTable(ctx, table)` — deletes the blob and name key.

### Transaction-scoped DDL and the backfill contract

One DDL operation is exported in a caller's-transaction form: the
package-level function

```go
func AddIndexIn(ctx, view kv.KV, tableName string, def IndexDef) (Table, Index, error)
```

mutates the catalog against `view` — expected to be a transaction the caller
owns — and returns the updated table plus the new index (which is visible
only inside that transaction until commit). It exists because index
registration and index backfill must be atomic: a committed index with no
entries would let the planner choose an access path that silently drops rows,
violating the invariant that access-path choice never changes results.
`exec.Engine.AddIndexWithBackfill` (rad/05_exec/backfill.go) is the
composition: inside one `Engine.Txn` it calls `catalog.AddIndexIn` and then
writes index entries for every existing row. A backfill failure — notably a
uniqueness violation — rolls the registration back with it, and the
serializable scan closes the race with concurrent inserts. `Catalog.AddIndex`
is `AddIndexIn` in its own transaction, for empty tables. The frontend's
migration applier always routes `AddIndex` steps through
`AddIndexWithBackfill`.

`mutateTableIn` (unexported) follows the same pattern for the other
mutations but is not exposed; `AddIndexIn` is the only *In variant in the
public surface today.

## Validation: what the catalog enforces, what it leaves above

`CreateTable` and the evolution operations enforce definitional consistency,
each rule pinned by a case in `TestCreateTableValidation` or the DDL tests:

table names are required and unique database-wide; column names are unique
within a table and types must be one of `TypeText`, `TypeInt64`,
`TypeFloat64`, `TypeBool`; column defaults must fit the column
(`validateDefault`: `uuid()` on text only, `now_ms()` on int64 only, unknown
generators rejected); a primary key is required, its columns must exist and
be non-nullable; indexes need at least one existing column; foreign keys must
reference an existing table's *full primary key, in order* (a POC
restriction), with matching column counts and types. Evolution adds:
`AddColumn` requires new columns to be nullable or carry a *literal* default,
because existing rows need a stable value (a generator would fabricate a
different one on every read); `DropColumn` refuses columns used by the
primary key, any index, or any of the table's own foreign keys — drop the
dependent object first.

Everything value-level is deliberately left to 05_exec: NOT NULL enforcement
on writes, unique-index checking, FK parent-existence on insert and
child-existence on delete (`checkForeignKeys`, `checkNoReferences`), default
materialization, and index maintenance. The catalog also does *not* guard
`DropTable` against inbound foreign keys from other tables — dropping a
referenced parent leaves children with dangling `RefTableID`s. Only the
migration differ checks this (`migrate: table %q references dropped table`);
direct `Catalog.DropTable` calls are unguarded.

## Subpackages: schema and migrate

Two subpackages round out the layer; both are frontends *to* the catalog and
depend on nothing above layer 02.

`schema` parses declarative `schema.rad` files (YAML, structurally validated
against the embedded JSON Schema `radschema.json`) into `catalog.TableDef`s.
It desugars column-level shorthands — `pk`, `unique`/`index` (which derive
deterministic index names via `indexName`, e.g. `users_email_uq`),
`ref: table.column` — and separates `renamed_from` hints into
`schema.Table.RenamedFrom`/`ColumnRenames`, keeping migration hints out of
the definition proper.

`migrate` is a pure differ: `Diff(current []catalog.Table, desired
*schema.Schema) ([]Step, error)` returns an ordered plan (renames, creates in
FK-dependency order, added columns, index drops, index adds, column drops,
table drops) without touching the store. Renames are recognized only through
hints — otherwise a rename diffs as drop-plus-add. Indexes are matched
structurally (post-rename columns + uniqueness), not by name, so a column
rename never forces a needless drop-and-backfill. Unsupported changes (column
type or nullability, primary keys, FKs on existing tables) are errors, not
destructive guesses. Applying the plan is the frontend's job
(`DB.Migrate` in rad/06_frontend/migrate.go), one step per transaction — the
plan as a whole is *not* atomic; a failing step aborts mid-plan with prior
steps committed.

## Invariants downstream layers rely on

- IDs are stable for the life of an object and never reused; all data keys
  are built from IDs, so any committed `Table` value, however stale, points
  at real keys.
- A committed index has entries for every committed row of its table (the
  `AddIndexIn` + backfill contract).
- `Table.PrimaryKey` is non-empty and its columns are non-nullable — the
  executor's tuple encoding and the binder's uniqueness-based cardinality
  inference both assume it.
- Column order in `Table.Columns` is the declaration order and round-trips
  through storage.
- FK shape: `Columns` and `RefColumns` have equal length, types match, and
  `RefColumns` is exactly the referenced table's primary key.
- `catalog.Type` is the engine-wide scalar vocabulary: `lir.Kind` is defined
  by direct conversion from it (`KindText = Kind(catalog.TypeText)`), and
  `lir.Value` carries a `catalog.Type` tag. Adding a type starts here.

## Testing

The layer is tested as executable documentation against a real store:
`catalog_test.go` and `ddl_test.go` open an in-memory SlateDB
(`kvslate.Open(name, "memory:///")`) per test, so JSON persistence, the
name index, and transactional atomicity are exercised for real rather than
mocked. `TestCreateTableValidation` is the validation matrix as a table of
rejected definitions, each asserting atomic rollback; the DDL tests pin the
identity-preservation invariants (rename keeps IDs, freed names are
reusable). `schema` tests parse fixtures through the real JSON Schema;
`migrate` tests build a live catalog, then assert `Diff`'s plans by their
`String()` forms — the differ itself never needs a store beyond producing
`current`.
