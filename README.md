# rad — an ORM-native relational database (POC)

A proof-of-concept relational database in Go whose product is the developer
experience, not the SQL dialect:

```
schema.rad → rad migrate → rad generate → typed Go client → QIR → planner → SlateDB → nested JSON
```

You declare a schema (YAML, JSON-Schema validated), reconcile the database
against it, generate a typed client, and build applications that never
touch SQL — evolving the schema regenerates the client and the Go compiler
flags every place the application must adapt. See `demo/` for a complete
product (a team task tracker) built this way, and `docs/v0-spec.md` for the
goals and non-goals.

Underneath: typed tables, primary keys, secondary indexes, foreign keys,
and unique constraints mapped CockroachDB-style onto order-preserving byte
keys over [SlateDB](https://slatedb.io); optimistic transactions
(SlateDB's SerializableSnapshot) make writes atomic and constraint checks
sound under concurrency.

**Not production-ready by design.**

## Quickstart

```
task demo      # migrate + run the Tracker demo app (demo/data)
task up        # demo + the devtool UI at http://127.0.0.1:7423
```

First run on a fresh machine: `task slatedb:setup` once, to build the
native SlateDB library. The full loop by hand:

```
rad migrate  -f demo/schema.rad -d demo/data
rad generate -f demo/schema.rad -o demo/generated --pkg tracker
cd demo && go run .
```

## Architecture

RAD is composed of several logical layers:

```text
Frontend
↓

QIR

↓

Planner

↓

Executor

↓

KV Storage
```

The frontend may consist of:

- generated ORM clients
- administrative tools
- SQL frontend (optional)
- future custom DSLs

All frontends compile into the same Query Intermediate Representation (QIR).

The planner converts QIR into physical execution plans.

The executor performs those plans against the underlying ordered key-value store.

---

### Storage

RAD is built on an ordered key-value abstraction.

The storage layer provides only low-level primitives:

- Get
- Put
- Delete
- RangeScan
- PrefixScan

The current implementation targets SlateDB.

The storage layer should remain unaware of relational semantics.

---

### Schema

Schemas are declarative.

Users do not issue DDL commands.

Instead, users define the desired schema and RAD computes the required catalog changes.

The schema is the single source of truth for:

- migrations
- generated clients
- type information
- relationships
- indexes
- constraints

The schema may eventually become a versioned contract distributed to clients.

---

### Migrations

Migrations are produced by diffing schema versions.

RAD internally performs catalog operations such as:

- create table
- add column
- remove column
- rename column
- create index

These operations are implementation details.

Users primarily work with schema revisions rather than imperative migration scripts.

Migration hints may be introduced where changes cannot be inferred automatically.

---

### Query Interface (QIR)

QIR is a structured JSON representation of relational operations.

It is designed for machines rather than humans, although it should remain readable and debuggable.

QIR is:

- versioned
- canonical
- strongly structured
- transportable
- language independent

Parameters are separated from query structure to eliminate the primary SQL injection class of bugs.

The QIR is the contract between frontends and the database.

---

### Result Shape

RAD separates data access from result materialisation.

Traditional SQL joins produce flat row sets requiring client-side reconstruction.

RAD instead allows queries to describe nested result shapes matching application data structures.

The planner remains free to execute joins, batched lookups or index scans as appropriate.

Applications receive structured JSON.

---

### Commands

RAD distinguishes between:

- Queries (read operations)
- Commands (mutating operations)
- Transactions

Mutations are not considered queries.

Commands may optionally return structured data.

---

### SQL

SQL is not the native interface.

An optional SQL frontend may compile a subset of SQLite-like SQL into QIR.

SQL compatibility is not a design objective.

---

### Type System

The core storage type system remains intentionally small.

Initial primitive types:

- String
- Int64
- Float64
- Bool

Columns may additionally expose semantic format metadata, similar to JSON Schema or OpenAPI:

```yaml
type: Int64
format: unix_ms
```

Formats communicate intent to generated clients and tools without increasing storage complexity.
