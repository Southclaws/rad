# Rad V0 — POC End Goal

> The goal of the POC is **not** to build a database. The goal is to prove
> the _developer experience_.
>
> Success criterion: "I can define a schema, run the database, generate a
> client, write an application against it, evolve the schema, regenerate the
> client, and never write SQL."

## The One Thing To Obsess Over

```text
schema.rad
      │
      ▼
rad migrate
      │
      ▼
rad generate
      │
      ▼
import db from "./generated"
      │
      ▼
Typed application code
      │
      ▼
Generated LIR
      │
      ▼
Planner
      │
      ▼
SlateDB
      │
      ▼
Nested JSON
```

## V0 Goals — Foundation

- Ordered KV abstraction over SlateDB
- Simple transaction support (whatever SlateDB provides)
- Declarative schema
- Schema reconciliation/migration engine
- Generated ORM client
- LIR execution pipeline
- JSON API

## Storage

Implement only: tables, primary keys, secondary indexes, foreign keys,
unique constraints.

Ignore: views, triggers, stored procedures, materialized views, extensions,
partitioning, replication, distributed execution, statistics, cost
optimizer.

## Types

Support only: String, Int64, Float64, Bool, Nullable.

Optional metadata: `format` (e.g. `String + format=email`,
`Int64 + format=unix_ms`, `String + format=uuid`).

Ignore: JSON, arrays, timezones, decimal, geometry, CIDR, money, XML,
custom types.

## Schema

Support: tables, columns, indexes, foreign keys, unique constraints,
formats, defaults (optional).

Ignore: check constraints, generated/computed columns, triggers, policies,
multiple schemas/namespaces.

## Migrations

Support: schema diff; add/remove table; add/remove column; rename hints;
add/remove indexes.

Ignore: data migrations, online migrations, complex rewrite planning,
rollback planning.

## ORM Generation

Generate: models, CRUD, relationships, query builders, commands,
transactions.

Ignore: lazy loading, identity maps, change tracking, magic.

## Query Interface (LIR)

Reads: `get`, `read`, `list`.
Commands: `add`, `change`, `remove`.
Transactions: `tx`.

Support: filtering, ordering, pagination, nested relationships, arbitrary
joins, returning, parameters.

Ignore: recursive queries, aggregates, group by, window functions, unions,
CTEs, functions, expression language beyond basics.

## Result Shape

Nested JSON:

```text
User
 ├── Profile
 └── Orders[]
```

Never expose flattened SQL-style joins by default.

## Planning

Support: PK lookup, secondary index lookup, full scan, nested relation
fetch, simple join, returning.

Ignore: cost optimization, statistics, join reordering, adaptive planning,
parallel execution.

## SQL

Tiny SQLite-ish subset (`SELECT FROM WHERE ORDER BY LIMIT JOIN`), compiled
only to LIR. No SQL endpoint required for v0.

Ignore: SQL compatibility, DDL, window functions, recursive CTEs, vendor
syntax.

## API

JSON only. Requests: Query, Command, Transaction. Responses: Object, Array.

## Developer Experience — must prove

✅ Define schema → ✅ Start DB → ✅ Generate ORM → ✅ Write app →
✅ Run migrations → ✅ Regenerate ORM → ✅ App still compiles →
✅ Execute LIR → ✅ Receive nested JSON → ✅ Never manually write SQL

## Ignore Completely

Performance, benchmarks, SQL compatibility, distributed execution, query
optimization, replication, permissions, authentication, RLS, stored
procedures, triggers, JSON querying, arrays, full text search, analytics,
OLAP, recursive queries, window functions, decimal correctness, date/time
correctness, timezones, prepared query caching, query fingerprint
allowlists (later), live schema propagation, hot schema reload.
