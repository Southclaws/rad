# Data mutation and transaction protocol

Status: design task — the v0 one-row CRUD API works and is useful for the
demo, but its prototype assumptions must become explicit before it hardens
into the generated-client contract. Added 2026-07-13 from a protocol review,
then grounded in the current implementation.

## Where we stand

The existing surface is deliberately small and coherent:

- `POST /create`, `/update`, and `/delete` perform one keyed mutation against
  committed state; `Create` returns the stored row, while update/delete use
  `found` for a missing key.
- `POST /tx` opens an explicit transaction; `/tx/{id}/{query,create,update,delete}`
  runs one operation within it, and commit/rollback finish it.
- The engine has the right initial execution boundary: every standalone write
  is a `SerializableSnapshot` transaction, and an explicit transaction sees
  its begin snapshot plus its own buffered writes. The storage abstraction
  tracks point reads and requested scan bounds, so the contract is more than
  optimistic write-write checking.
- A session is currently an in-process map entry containing a `*frontend.Tx`.
  It is protected by a per-session mutex, reaped after 60 seconds of
  inactivity, and removed before commit/rollback runs. A fresh random 128-bit
  ID is the only handle. Rad itself has no authentication; production auth is
  expected to be supplied by a TLS-terminating proxy.

The implementation is therefore correctly simple for one server, but it has
not yet chosen a distributed-session, retry, or mutation-composition model.
The OpenAPI contract and generated clients repeat each data operation for a
transactional path, so the choice will get more expensive with every new
operation.

Relevant code:

- `rad/api/openapi.yaml` — current public CRUD and `/tx/{id}/...` contract.
- `rad/server/api/sessions.go` — local session registry, locking, expiry and
  random IDs.
- `rad/server/api/dbserver.go` — duplicated transport handlers, which share
  `doCreate`, `doUpdate`, `doDelete`, and `doQuery` below the route boundary.
- `rad/engine/01_kv/kv.go` and `rad/engine/05_exec/engine.go` — isolation
  guarantee and the `SerializableSnapshot` begin path.
- `rad/engine/05_exec/mutate.go` — point mutations, immediate constraint
  checks, immutable primary keys, and restrict-only deletes.
- `rad/client/client.go` — generated-client-shaped convenience methods and
  current retry/rollback behaviour.

## Decisions to make before expanding this API

### 1. Make the transaction deployment contract honest

An explicit HTTP transaction is stateful even though the transport is HTTP.
Today a request must return to the process that owns its `*frontend.Tx`; a
round-robin load balancer will otherwise turn valid session IDs into
`not_found`. Process loss also loses the session and its uncommitted work.

Choose and document one v0 deployment rule:

1. **Single Rad process per database** — the cleanest current contract; a
   proxy may sit in front of it but must not distribute a database across Rad
   instances.
2. **Pinned/owner-routed sessions** — a gateway or load balancer routes all
   requests for a transaction back to its owner. The transaction is aborted
   if that owner dies.

Do not attempt to serialize or share live SlateDB transaction state through
the KV store merely to make HTTP look stateless. If multi-instance execution
is a product goal, define owner routing, principal binding (once Rad has an
auth identity), failure behaviour, and observability as a separate design.
An ID should remain opaque to clients; it need not encode routing metadata in
the public format.

### 2. Preserve the actual isolation claim — and prove it at the storage seam

The current engine deliberately calls `kv.SerializableSnapshot`, whose
documented read-set/range validation is intended to prevent write skew and
phantoms. That is a credible serializable guarantee *only if every supported
KV backend implements that contract and the executor records every semantic
read through it*.

Keep the word “serializable” only with those conditions documented and tested:

- a write-skew test (the classic two-doctors-on-call case);
- a phantom/range test where a transaction observes an empty predicate range
  and a concurrent transaction inserts into it;
- constraint-race tests for duplicate unique values and FK parent deletion;
- backend conformance tests, not only in-memory SlateDB tests.

If a future backend can offer only snapshot isolation, expose that difference
in its capability/configuration rather than silently weakening the HTTP
promise. Commit conflicts are retryable serialization conflicts; they are
not the same as a request that violates a declared constraint.

### 3. Define transaction lifecycle and ambiguous terminal outcomes

`finish` currently removes the session before invoking `Commit` or `Rollback`.
After a lost commit response, a retry gets the same `not_found` response as an
unknown ID, expiry, rollback, or a process crash; the client cannot learn
whether the write committed.

Specify a bounded terminal record before treating `Commit` as retry-safe:

```text
active → committed | rolled_back | aborted | expired
```

Retain terminal status for a short, configurable retention period. A repeated
commit of `committed` should be successful; a commit after rollback/expiry
should return a distinct invalid state; an unrecognised ID remains not found.
The design may use a status endpoint or make terminal responses idempotent,
but it must state the retry outcome. The terminal record cannot resurrect a
lost in-memory transaction after process failure; that case needs its own
`aborted`/unknown semantics.

Turn the current “roughly sixty seconds” into a lease contract:

- return the expiry/idle timeout at begin;
- say that a successful in-session operation refreshes the lease (or name the
  operations that do);
- decide whether a long-running operation may race expiry;
- make idle timeout, maximum age, session count, staged bytes/writes, read-set
  size, and execution time configurable resource limits;
- add keepalive only if a real client workflow needs application work between
  statements. A short lease is desirable pressure against holding database
  resources while waiting on users.

### 4. Make execution context orthogonal to operations

The current duplicate route tree is already mechanically sharing helpers, but
the OpenAPI surface, generated OAS client, and hand-written client switches
all duplicate it. Query, explain, streaming, batch, or future mutation
operations would multiply that work.

Pick one common transaction-context mechanism before the next operation is
added:

- a `Rad-Transaction` request header; or
- a common operation envelope with an optional transaction ID.

The header keeps operation bodies unchanged and keeps a live session handle
out of URLs that routine proxy/access logs, history, and tracing commonly
record. It also avoids two endpoint trees. This is a protocol break, so do it
only as one deliberate v0 reshape with regenerated OAS/clients and migration
notes; do not introduce an adapter layer that makes both trees permanent.

### 5. Establish a small mutation IR and atomic batch boundary

Do not let the current `RowUpdate` name imply that a point update is Rad’s
only possible update. Internally name the existing primitives by their real
scope:

```text
Create
UpdateByKey
DeleteByKey
```

Then make the execution boundary able to accept an ordered list:

```text
Mutate([]Mutation) -> []MutationResult
```

A one-operation request uses a fresh atomic transaction; a request bearing a
transaction context stages the same list in that transaction. Initially, a
batch should be ordered, atomic as a whole, bounded, and *not* allow results
to feed later operations. That removes request/auth/transaction overhead for
imports and parent-child writes without prematurely designing dataflow.

Keep point mutation as the initial public capability. Relational
`UpdateWhere`/`DeleteWhere`, upsert, expression assignments, result bindings,
and arbitrary `returning` should wait: they require explicit snapshot,
materialisation, ordering, size-limit, result-shape, and update-affects-
predicate semantics. A future relational mutation should target a relation
expression rather than grow a second ad-hoc condition language.

### 6. Freeze the v0 statement and constraint rules

The executor already performs constraint checks against the transaction KV
view, so parent-before-child creation and child-before-parent deletion work
within one transaction, while the inverse ordering fails. State this as the
v0 model: constraints are immediate, no constraints are deferrable, and
restrict is the only supported FK delete action. Model `restrict` as the
currently supported referential action, not as an eternal global law.

Keep the errors distinct in the error-propagation work:

```text
invalid / constraint_violation    # cannot succeed without data/request change
conflict / serializable_conflict  # retry the whole transaction may succeed
```

Do not conflate missing-key results (`found: false`) with precondition
failure. Add a narrow keyed precondition (`expected` column/value pairs or a
reused scalar predicate) before presenting non-transactional read-modify-write
as safe optimistic concurrency. Its result needs three states: updated,
not_found, and condition_not_met.

### 7. Close the immediate point-CRUD edge cases

These are small contract gaps worth fixing independently of the larger
reshape:

- Reject overlap between `set` and `clear`. Today the transport silently lets
  `clear` win because it writes NULL into the coerced set map after decoding.
- Specify duplicate/unknown clear columns, nullable checks, generated/default
  assignment semantics, and the immutable-primary-key rule in OpenAPI.
- Keep composite primary keys as a cell map, as the current API does.
- Do not freeze “return the complete row” as an eternal invariant. Batch and
  wide-row use cases eventually need `none`, `key`, `row`, or projection
  returning; generated clients can retain the pleasant single-row helpers.
- Non-transactional create/update/delete need an idempotency policy before
  automatic retry is encouraged. A client-supplied idempotency key with a
  bounded result record is the likely mechanism, especially for server-
  generated primary keys.

## Proposed delivery order

1. **Contract tests and documentation first.** Capture actual current
   isolation, read-your-writes, immediate constraints, lease refresh, expiry,
   owner loss, and ambiguous commit expectations. Decide the single-node vs
   owner-routed v0 deployment rule.
2. **Tighten current point CRUD.** Validate `set`/`clear` disjointness and
   document immediate constraints, restrictive FK deletion, immutable keys,
   and the separate conflict/constraint reasons.
3. **Transaction protocol reshape.** Add lifecycle states and bounded
   terminal status, then move the transaction ID into one shared execution
   context and regenerate the clients.
4. **Mutation IR plus bounded atomic batch.** Lower existing one-row methods
   to it; preserve their generated-client ergonomics while removing duplicate
   server/client operation handling.
5. **Only from concrete demand:** keyed preconditions and idempotency,
   returning projections, relational mutations, alternate encodings/streaming,
   or multi-instance routing.

## Non-goals for this task

- Replacing HTTP/JSON with a custom TCP protocol. HTTP/2 plus a semantic batch
  boundary is the relevant early optimisation; streaming, compression,
  alternate media types, prepared plans, and HTTP/3 can preserve the same
  semantics later.
- Distributing live transactions through the database.
- Adding arbitrary bulk predicates, cascades, deferred constraints, or an
  all-purpose mutation language before their execution semantics are chosen.

Related: `tasks/1-todo/error-propagation.md` (the settled
`constraint_violation` / `serializable_conflict` taxonomy); `rad/api/openapi.yaml`
(the contract that will need one intentional revision); and the KV backend
conformance tests that must carry the isolation guarantee.
