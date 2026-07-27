# Concurrent outside-in tests

This suite runs Rad's real HTTP and PostgreSQL servers over one in-memory
SlateDB and attacks them from independent client connections. A deterministic
director releases one round of operations together, then waits for the whole
round before releasing the next. The ordering inside a round is deliberately
left to Go, the network stacks, and SlateDB.

Each scenario mixes:

- HTTP create, update, delete, and full-snapshot reads;
- PostgreSQL serializable transactions, read-your-write checks, repeatable
  snapshot reads, and case-insensitive `ILIKE` queries;
- concurrent metadata-only table/column renames and nullable-column additions;
- an online index build automatically discovered and fairly advanced in tiny
  durable batches while mutations emit deltas, including duplicate index keys;
- automatic column-replacement and not-null-validation workers under mixed
  HTTP/PostgreSQL reads and writes;
- a PostgreSQL writer admitted before index capture that must fail with
  `40001`, followed by a successful whole-transaction retry;
- transition inspection during the build;
- administrative cancellation overtaking an already-claimed automatic index
  worker, proving the stale worker cannot revive the transition or leave a
  foreground write obligation behind;
- automatic table reclamation in bounded batches while unrelated HTTP and
  PostgreSQL readers/writers continue, followed by checks that the current
  physical ranges are empty while a pre-delete Slate snapshot still sees its
  retained row and index versions;
- quiescent model, base-table/index differential, catalog, and planner
  visibility checks after the traffic stops.

The final index auditor uses a scheduling-disabled in-process engine handle.
The actual schema worker is an automatic job runner; application traffic,
transition starts, and transition inspection cross the real PIR and OpenAPI
frontends. Tests never invoke worker claim/step methods or write SlateDB keys
directly. Exact semantic schedules may keep the server's runner disabled until
the application transaction is parked at a chosen boundary, then attach a
separate automatic runner over the same durable database and schedule its
ordinary worker yields.

The suite also has a (somewhat) deterministic semantic scheduler. Engine hooks
are inert in production and may hold work at catalog pinning, binding,
dependency-fence admission, catalog publication, commit, owner takeover,
transition batch, and finalization boundaries. Schedules under
`testdata/schedules` identify the actor, boundary, object, and occurrence;
replay must reproduce the recorded schedule byte-for-byte. A bounded serial-
history oracle explores every ordering allowed by invocation/response time for
rename, create/delete, ordinary writes, build start, ready publication, and
coherent observations.

Online unique-index coverage drives independent HTTP writers while the real
automatic worker scans, catches up, gates only the affected table, validates
durable claims, and publishes. It verifies the ready index contains no
duplicates and PostgreSQL observes `23505` after publication.

Run the default scenarios:

```sh
go test ./tests/concurrent
```

Replay every scenario with a particular director seed or increase its rounds:

```sh
RAD_CONCURRENT_SEED=7331 RAD_CONCURRENT_ROUNDS=50 \
  go test ./tests/concurrent -run mixed-traffic -count=1
```

Run many scheduler interleavings of the same coarse schedule:

```sh
go test ./tests/concurrent -run mixed-traffic -count=100
go test -race ./tests/concurrent -run mixed-traffic -count=10
```

Run the exact semantic replay and bounded history oracle:

```sh
go test ./tests/concurrent -run 'TestSemanticSchedule|TestBoundedSerialHistory'
```

Set `RAD_CONCURRENT_JOURNAL` to a directory to persist the scenario plus the
ordered release, retry, and completion journal as JSON. Failures also print
the seed, replay command, and the journal tail.

These tests are intentionally pre-DST. They make concurrency broad and
repeatable and can exactly replay selected semantic boundaries, but not every
goroutine, network, storage, or timer instruction. They do not inject torn
storage, clock changes, or network partitions.
