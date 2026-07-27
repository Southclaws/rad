# Rad engine architecture and operations

This directory is the storage-independent core of Rad. It turns PIR programs
and LIR queries into catalog-bound plans, executes them in Slate transactions,
and owns background schema work and physical reclamation.

The numbered packages are dependency layers. Code may depend downward, never
upward:

| Layer | Responsibility |
| --- | --- |
| `01_kv` | Transaction and key-value abstractions, Slate adapter, key encoding |
| `02_catalog` | Durable catalog model, storage, mutations, schema, migration |
| `03_lir` | Pure relational meaning and bound expressions/relations |
| `04_planner` | Binding, dependency analysis, physical planning, explain |
| `05_exec` | Transaction façade, operators, mutations, schema jobs, reclamation |
| `06_frontend` | Application-facing façade and result conversion |

The PostgreSQL server and HTTP API are adapters above these layers. They do not
define engine transaction or catalog semantics.

## The two version histories

Catalog consistency uses several deliberately different identities and
counters. They must not be replaced by one global “schema version” check.

**Canonical schema revision** is the high-level, monotonically increasing
history of the complete logical schema. Direct catalog changes publish a new
revision. It supports schema inspection, migration, hashing, and audit history,
but an ordinary data transaction does not abort merely because this number
changed.

**Schema ID** is the stable logical identity of a table or column. Names may
change while schema IDs remain stable, so already-bound work can survive a
rename.

**Physical ID** identifies stored table, column, and index data. A physical ID
is opaque and never reused. A column type/encoding replacement receives a new
physical ID instead of reinterpreting old bytes.

**Definition generation** versions one catalog object's meaning. Immutable
generation-qualified definitions let an executing transaction retain exactly
the table or column semantics it bound.

**Write-protocol generation** versions a table's foreground mutation
obligations. It changes when writers must maintain a new ready index, emit a
transition delta, dual-write a replacement column, enforce a constraint, or
observe a finalization gate.

**Transition generation** versions a durable schema job's state and progress.
It is diagnostics and worker state, not a plan compatibility rule.

**Owner epoch** fences workers. Claiming a job advances its epoch; a stale
worker may finish local computation but cannot commit another batch.

These values answer different questions:

| Question | Identity or counter |
| --- | --- |
| What complete schema was published? | Canonical schema revision |
| Is this still the same logical object after rename? | Schema ID |
| Which stored representation is this? | Physical ID |
| Does the object still mean what this plan bound? | Definition generation/fence |
| Must this writer perform the same obligations? | Write-protocol generation |
| Is this worker state/checkpoint current? | Transition generation |
| May this worker still publish? | Owner epoch |

## Catalog MVCC transaction path

An executing transaction follows this order:

```text
pin one coherent immutable catalog snapshot
  -> bind names to stable identities
  -> produce a typed dependency manifest
  -> begin the Slate data transaction
  -> admit every dependency fence inside that transaction
  -> execute using only pinned definitions and write protocols
  -> commit through Slate conflict detection
```

The manifest records the exact semantics used: table existence, decoded
column values, selected index access, and mutation write protocols. A projected
read does not depend on columns it never observes; `count(*)` need not depend
on any column value definition. Consequently an unrelated catalog publication
does not invalidate the plan.

Fence admission after beginning the data transaction is essential. It closes
the race between catalog pinning and Slate snapshot creation. Slate's
serializable commit detection closes a later race between fence admission and
commit. Rad does not implement another row-version store above Slate.

Catalog work inside a PIR program refreshes the program's pinned catalog for
following statements. Ordinary data statements continue to share the same
transaction and atomic program boundary.

## Catalog changes and schema transitions

Catalog changes fall into three operational classes:

1. **Metadata-only changes** publish in one short transaction. Renames and
   adding many nullable columns use stable identities and sparse-row semantics,
   so they require no row rewrite.
2. **Compatibility-sensitive changes** publish new definitions and fences.
   Only work whose manifest used the changed semantics conflicts.
3. **Physical transitions** publish durable work, perform bounded resumable
   batches outside the catalog critical section, validate, then publish the
   logical result in another short transaction.

A **schema transition** is the durable state machine for the third class. It is
not the catalog change itself and it is not a SQL transaction. Current kinds
cover online index builds, column replacement, and constraint validation.

```text
waiting -> building -> catching_up -> validating -> ready
                     \                         \-> failed
                      \------------------------> cancelled
```

Not every kind visits every state. `waiting` means prerequisites are durable
but no foreground obligation has been installed. Activation rebinds the stored
logical request against current physical representations. `ready`, `failed`,
and `cancelled` are terminal outcomes.

The long-running worker is deliberately separate from the start statement:

- the start commits the logical definition, transition record, capture
  obligation, and compatibility fences atomically;
- each worker batch commits physical work and its checkpoint atomically;
- final publication validates the caught-up representation and changes its
  planner visibility atomically.

Starting online schema work is a transactional PIR operation because it may be
ordered with other catalog and data statements. Listing, inspecting, and
cancelling that durable work are administrative OpenAPI operations. They use
the same engine records, but they are deliberately not PIR statements:
inspection is a coherent read-only snapshot, and cancellation is its own short
atomic catalog transaction rather than an operation interleaved with an
application program. A future admin UI can poll or stream this administrative
surface without expanding PIR into a job-control protocol.

Only ready indexes are planner-visible. Building, catching-up, validating,
deleting, failed, and cancelled indexes cannot be selected by new plans.

## Write protocols and finalization gates

A **write protocol** is an immutable, generation-qualified description of
everything a mutation of one table must do. Writers bind one complete protocol
instead of reconstructing obligations from mutable transition records.

A **finalization gate** is a short exclusive write gate stored in that
protocol. One table can be gated by at most one transition. A transition that
affects multiple tables installs one gate in each affected table, in canonical
physical-ID order, in one Slate transaction.

The gate is used only for a short validation/publication phase, never for the
base scan or backfill. Writers admitted under the prior protocol either commit
before gate publication or conflict. Later writers receive a stable retryable
“transition finalizing” rejection. Unrelated tables remain writable.

Multiple compatible transitions may scan and capture changes on the same table
concurrently because their delta sinks, dual-writes, and checks compose in the
write protocol. Only incompatible activation or publication phases serialize.

## Schema jobs

Each engine owns a local scheduler for durable schema transitions,
reclamations, and catalog-history compaction. Slate's single-writer model means
one Rad process owns a database; owner epochs still protect against stale work
within the process and across restart.

The scheduler:

- discovers durable unfinished work through sticky work markers;
- advances a rotating bounded window, at most one batch per selected job per
  round;
- applies logical item budgets and capped backoff;
- treats expected conflicts, retention waits, and backpressure as retryable;
- quarantines repeated unexpected failures rather than spinning; and
- resumes work after a file-backed close/reopen.

Batch size, scheduling interval, and process-local budgets are resource policy,
not correctness state. A caller may use explicit `Run*` and `Step*` methods for
testing or embedding, but normal operation needs no worker-control loop.

## Logical deletion, retention, and reclamation

Deleting a catalog object is logical first. New binding stops seeing it
immediately; its physical bytes remain until no retained consumer can require
them.

A **retention pin** is a durable typed claim that a resource must remain
available. Pins cover exact catalog definitions, data snapshots, transition
deltas/diagnostics, and physical table/column/index artifacts. A
resource-specific **horizon** is the oldest generation or position still
protected by active pins.

A **reclamation** is a durable, owner-fenced, resumable physical cleanup job.
Before every batch it proves that its exact target is still retired and checks
for a blocking pin. It then deletes or rewrites at most its configured item
budget and commits the cursor with the physical changes. Reclamation never
depends merely on age or on the latest canonical schema revision.

```text
logical delete or transition retirement
  -> durable reclamation record
  -> wait behind resource-specific pins
  -> bounded physical batches
  -> reclaimed terminal record
  -> compact detailed diagnostics when retention permits
```

This is automatic engine housekeeping. Users do not run `VACUUM`, schedule a
cron job, or manually expose a safe horizon. Storage-pressure metrics report
retained and reclaimable work so operators can diagnose pins or stalled jobs.
Canonical schema-history compaction is independent: deleting old audit
revisions cannot make a current definition unbindable and cannot authorize
physical reclamation.

## Durable JSON and internal types

Deep engine structs should not gain JSON tags by default. Tags on catalog
tables, immutable definitions, write protocols, transitions, pins, and
reclamations describe durable values stored under `/rad/catalog/...` Slate
keys. They are storage schemas, not HTTP or PIR contracts. Comments beside
such types must identify that boundary. Pure in-memory planning and execution
types should remain untagged. Correctness-bearing operational records fail
closed on unknown fields, trailing JSON values, invalid lifecycle state, and
key/payload identity drift so an older binary cannot silently ignore a newer
storage invariant after downgrade.

## Failure categories

Callers and workers must preserve these distinctions:

- invalid input or catalog drift is deterministic and not retryable;
- a Slate serialization conflict is retryable from a fresh transaction;
- finalization and transition backlog are scoped retryable rejections;
- retention blocking is expected waiting, not job failure;
- cancellation and failed validation are durable terminal outcomes; and
- an unknown storage/commit outcome is not equivalent to a serialization
  conflict.

The engine does not currently retry arbitrary PIR programs server-side. A
transport adapter may retry only when it owns the complete transaction unit
and the error contract proves the attempt did not commit.

## Where to read next

- [`02_catalog/model`](02_catalog/model) defines durable identities, lifecycle
  states, write protocols, pins, and reclamations.
- [`02_catalog/store`](02_catalog/store) defines durable keys, immutable
  definitions, fences, and canonical encoding.
- [`02_catalog/change`](02_catalog/change) contains atomic catalog mutations
  and schema-transition publication rules.
- [`04_planner/plan.go`](04_planner/plan.go) carries bound dependency manifests.
- [`05_exec`](05_exec) owns transaction admission, workers, scheduling, and
  reclamation. Its [executor glossary](05_exec/README.md) defines relational
  and plan vocabulary.
- [`tasks/3-done/catalog-mvcc-and-online-schema-transitions.md`](../../tasks/3-done/catalog-mvcc-and-online-schema-transitions.md)
  contains the detailed design record and executable-evidence ledger.
