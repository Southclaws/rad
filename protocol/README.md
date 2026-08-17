# Protocol schemas

`lir.schema.yaml` and `pir.schema.yaml` are the authored sources of truth for
Rad's wire protocols. Schemancer generates their Rust representations into
`src/protocol/generated`; those files are checked in and must not be edited by
hand.

The schemas also drive cross-language codegen for official clients.

`storage.keyform` is the authored source of truth for the durable storage
formats: the binary keyspace, ordered key scalars, row bodies, and the laws
and byte vectors that pin them. The `keyform` workspace tool
(`tools/keyform`) generates the format manifest, the keyspace registry, the
typed key constructors and parsers, the key renderer, and the
`tests/storage_spec.rs` conformance suite. Regenerate with
`task generate:storage` and never edit generated files by hand.

The schemas under `storage/` are the normative contracts for structured
durable JSON values. Each keyspace names its schema in `storage.keyform`.
Storage schemas obey stricter rules than API schemas because old databases
can contain released values indefinitely: a released schema version is
structurally frozen. Any structural change — adding or removing a property,
changing a type, changing whether a property is required, adding or removing
an enum member — creates a new schema version (`table.v2`) instead of
changing what released bytes mean. Readers of a keyspace must then accept
every released version of its schema.

`storage.allocations` is the permanent allocation registry: the root magic,
keyspace tags, scalar tags, record magics, golden-vector digests, and schema
structural signatures. keyform maintains it and fails the build when an edit
changes a released meaning. A removed keyspace keeps its tag as a `reserved`
line forever.

Until the storage format freezes, nothing is released and there are no
compatibility guarantees. For a deliberate pre-freeze format change, run
`cargo run --package keyform -- --rebaseline` to rewrite the registry from
the current specification, and wipe existing stores. After the freeze,
this will be removed and keyform changes will warrant a new version+migration.

Durable formats evolve in four classes:

- **Additive.** New optional JSON properties, new keyspaces, new derived
  metadata. No existing bytes change.
- **Lazy payload migration.** Both payload formats are readable; new writes
  use the new format; keys do not change.
- **Rebuildable derived state.** Indexes and other derived structures get a
  new physical identity, backfill beside the authoritative one, catch up,
  validate, publish, and reclaim the old identity.
- **Authoritative physical generation.** Fundamental row or key changes
  allocate a new table storage generation, backfill it beside the
  authoritative one, and switch authority atomically. The old generation
  stays readable until reclamation.

In-place mutation is forbidden: the root magic, allocated tag meanings,
released codec byte semantics, tuple framing, canonical representations, key
comparison semantics, and the type bound to a physical column identity never
change under an existing allocation. Identities are never reused.
