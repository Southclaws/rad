# rad — an ORM-native relational database (MVP)

A client/server relational database in Go whose product is the developer
experience, not the SQL dialect:

```
schema.rad → rad migrate → rad generate → typed Go client → rad:// wire QIR → planner → SlateDB → nested JSON
```

One binary — `rad` — is the entire system: the database server (`rad
serve`, HTTP on port **7237**), the migration tool (`rad migrate`), and the
client codegen (`rad generate`). Applications link a pure-Go generated
client (no cgo, no SQL) and connect with `rad://host:7237` (`rads://` to
reach it through an HTTPS proxy). Storage is SlateDB in every mode —
in-memory, local file, or S3 — selected by environment variables. One Rad
instance is one database; two databases are two RADs.

See `demo/` for a complete product (a team task tracker) built this way,
and `docs/v0-spec.md` for goals and non-goals. **Proof of concept** — no
authentication yet; deploy behind a proxy you trust.

## Install

```
curl -fsSL https://raw.githubusercontent.com/Southclaws/rad/main/install.sh | sh    # linux/macOS
powershell -ExecutionPolicy Bypass -c "irm .../install.ps1 | iex"                    # windows
```

Releases are built for linux (amd64/arm64), macOS (amd64/arm64, unsigned),
and windows-amd64 by `.github/workflows/release.yml` on every `v*` tag.

## Run a database

```
rad serve                                   # file storage in ./data, port 7237
RAD_STORAGE=memory rad serve                # ephemeral
RAD_STORAGE=s3 RAD_S3_BUCKET=my-bucket \
  RAD_S3_REGION=eu-west-1 rad serve         # object storage (AWS_* creds)
```

| Variable          | Meaning                     | Default |
| ----------------- | --------------------------- | ------- |
| `RAD_ADDR`        | listen address              | `:7237` |
| `RAD_STORAGE`     | `memory` \| `file` \| `s3`  | `file`  |
| `RAD_DATA_DIR`    | file-mode store directory   | `data`  |
| `RAD_S3_BUCKET`   | s3 bucket (required for s3) | —       |
| `RAD_S3_PREFIX`   | s3 key prefix / db path     | `rad`   |
| `RAD_S3_REGION`   | s3 region (or `AWS_REGION`) | —       |
| `RAD_S3_ENDPOINT` | custom endpoint (MinIO/R2)  | —       |

The server follows standard HTTP practice (timeouts, body limits, panic
recovery, request logs, graceful shutdown) and serves errors as RFC 7807
problem+json. It speaks plain HTTP; put your own proxy in front and connect
with `rads://` to dial it over HTTPS. Docker: `task docker:build && docker
run -p 7237:7237 rad`.

## Build an app

```
rad migrate  -u rad://localhost -f schema.rad          # reconcile schema over the wire
rad generate -f schema.rad -o ./generated --pkg db     # emit the typed client
```

```go
db, _ := db.Connect("rad://localhost")
user, _ := db.Users.Create(ctx, db.UserCreate{Name: "ada"})
boards, _ := db.Boards.Query().
    IncludeTasks(func(t *db.TaskInclude) { t.DoneEq(false).IncludeAssignee() }).
    All(ctx)   // nested JSON in, typed structs out — zero SQL
```

## Quickstart (this repo)

```
task demo      # fresh server + the Tracker demo app over rad://
task up        # same, then keeps serving (devtool UI on :7237)
```

First run on a fresh machine: `task slatedb:setup` once, to build the
native SlateDB library (server builds only — client apps are pure Go).

## Architecture

Rad is composed of several logical layers:

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

Rad is built on an ordered key-value abstraction.

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

Instead, users define the desired schema and Rad computes the required catalog changes.

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

Rad internally performs catalog operations such as:

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

Rad separates data access from result materialisation.

Traditional SQL joins produce flat row sets requiring client-side reconstruction.

Rad instead allows queries to describe nested result shapes matching application data structures.

The planner remains free to execute joins, batched lookups or index scans as appropriate.

Applications receive structured JSON.

---

### Commands

Rad distinguishes between:

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
