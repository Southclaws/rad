# ADR: a SQL frontend that compiles to LIR+PIR

Status: proposed, not scheduled — a proof-of-concept/prototype, no code
yet, and nothing here is set in stone. Rough sequencing (2026-07-14):
**phase 1**, SQLite dialect via vendored `rqlite/sql`, a client-side
text-compile POC proving the AST→IR mapping layer; **phase 2**, Postgres
dialect via `pgplex/pgparser` plus a real `psql-wire`-based wire-protocol
frontend, so unmodified Postgres clients/ORMs/test suites can hit Rad
directly. See "Alternative: Postgres dialect and wire protocol" for the
phase-2 research. How phase 2 is actually deployed — embedded in Rad's own
server, or a standalone layer/service in front of it translating wire
protocol to Rad's existing API — is deliberately undecided; this is
experimental, and what matters right now is getting the SQL→IR
compiler itself working, not settling the surrounding architecture.

## Context

Rad has no SQL today: the only query surface is LIR/PIR JSON sent to
`POST /execute` (see `rad/protocol/lir.schema.yaml`,
`pir.schema.yaml`). That is deliberate — LIR is a relation algebra, not a
SQL-alike, and PIR is the effectful program layer above it
(`tasks/3-done/data-mutation-and-transaction-protocol.md`).

But SQL familiarity is worth having as a _frontend_, for two reasons:

1. It's the fastest way for someone to try Rad without learning LIR JSON
   first.
2. It opens a battle-testing path: point an existing project's real
   query/ORM traffic — or better, its existing test suite — at a SQL string
   compiled to Rad instead of at SQLite/Postgres/Turso, and get correctness
   pressure from a workload nobody hand-wrote for Rad. That is a much
   stronger signal than the planner/e2e fixtures we author ourselves, which
   only exercise what we already thought to test.

This ADR proposes a **SQL-to-IR compiler**, entirely client-side: a Go
package that parses a SQL string against a known schema and emits a
`protocol.Query` or `protocol.Program` — the same Go types the hand-built
LIR/PIR examples throughout this repo already construct
(`rad/protocol/build.go`, `tests/harness/harness.go`). No new server
endpoint, no wire format change. It's a translation layer that sits in
front of the existing client, not a new capability of the engine.

## Goals

- Compile a useful, well-defined _subset_ of SQL (SQLite dialect, since
  SQLite's grammar is the smallest "real" SQL and the union of syntax most
  embedded/ORM tooling actually emits) to `protocol.Query` (reads) and
  `protocol.Program` (writes).
- Client-side only: parsing, binding, and IR emission need the table
  schema (names, columns, types, nullability, primary keys, indexes,
  foreign keys) but nothing else from a live server. `Client.Tables(ctx)`
  already returns exactly that shape (`protocol.TableInfo`/`ColumnInfo`,
  `client.go:117`), and `schema.Parse` (`rad/engine/02_catalog/schema/
schema.go:126`) parses a local `rad.schema.yaml` file into the same
  information without a connection at all — either is a sufficient input to
  the compiler's binder. Compiling one query touches the network zero
  times beyond that one-time schema fetch.
- A separate, self-contained Go package — not wired into `rad/client`,
  `rad/server`, or the OpenAPI surface. It produces `protocol.Query`/
  `protocol.Program` values a caller can execute however they like (in-
  process against a `harness`-style test DB, or marshaled and posted to a
  real server) — what to do with it as a product surface is explicitly
  undecided and out of scope for this ADR.
- A concrete, runnable proof of concept: one representative query and one
  representative mutation compiling correctly end to end, with a
  differential test against a real SQLite engine as the correctness
  oracle (see Verification below) — not a parser skeleton with no mapping
  behind it.

## Non-goals

- **Not the full SQL spec.** Huge, and most of it (window functions,
  recursive CTEs, materialized views, stored procedures, full type-
  affinity emulation) is either a separate project or not worth it for an
  experimentation tool.
- **Not SQLite-bug-compatible.** SQLite's dynamic typing/type-affinity
  looseness (storing `'abc'` in an `INTEGER` column, `1 == '1'` weak
  coercions, etc.) is a known wart, not a feature to replicate — Rad's
  columns are strictly typed and that stays true through this frontend.
  Anything relying on that looseness won't compile, by design.
- **Not a new public interface — for phase 1.** No new HTTP endpoint, no
  OpenAPI surface, no generated-client integration; it's a Go package in
  the repo for experimentation. Phase 2 (a wire-protocol frontend, see
  "Alternative: Postgres dialect and wire protocol" below) does put
  something SQL-speaking in front of Rad, likely as its own separate
  layer rather than Rad's server itself — but even that's not decided; it's
  a prototype, not a product commitment.
- **Not transaction/session SQL.** No `BEGIN`/`COMMIT`/`SAVEPOINT`, no
  `PRAGMA`, no `SET`. PIR's atomicity model (a whole `Program` is the
  transaction) has no SQL-session equivalent to map onto, and session
  state isn't part of "a query compiles to IR."
- **Not recursive CTEs.** Tracked separately
  (`tasks/1-todo/recursive-queries.md`) because LIR has no recursion
  operator yet (`schema-flexibility.md`'s `recursive`/`recursive_ref`
  sketch). Non-recursive `WITH` is in scope (see below) — LIR's `bindings`
  already exist for exactly this.

## Why the mapping is tractable: LIR's shape already mirrors SQL's

This is the load-bearing observation the rest of the plan leans on. SQL's
_logical_ clause evaluation order — `FROM` → `WHERE` → `GROUP BY` →
`HAVING` → window → `SELECT` → `DISTINCT` → `ORDER BY` → `LIMIT`/`OFFSET`
— is already how LIR chains nodes: `scan`/`join` → `filter` → `aggregate`
→ `filter` (on the aggregate's output scope, for `HAVING`) → `project` →
`order` → `slice`. A syntax-directed compiler that walks the AST in that
order and emits one LIR node per clause is not fighting an impedance
mismatch; it's translating between two things that already agree on
structure. Scope-qualified columns (`{kind: col, scope, column}`) map
directly from SQL's `alias.column`, and SQL's subquery forms map close to
1:1 onto LIR's four crossings:

| SQL construct                                     | LIR construct                               |
| ------------------------------------------------- | ------------------------------------------- |
| `FROM t [AS alias]`                               | `scan` (table = `t`, scope = alias)         |
| `FROM a JOIN b ON ...` / `LEFT JOIN`              | `join` (`inner`/`left`)                     |
| `WHERE pred`                                      | `filter`                                    |
| `SELECT expr AS name, ...`                        | `project` (`fields`), `spread` for `t.*`    |
| `GROUP BY ...` / aggregate functions              | `aggregate` (`groups`/`aggs`)               |
| `HAVING pred`                                     | `filter` stacked on the aggregate's `scope` |
| `ORDER BY ...`                                    | `order`                                     |
| `LIMIT n [OFFSET m]`                              | `slice`                                     |
| `WITH name AS (...)` (non-recursive)              | `bindings` + `ref`                          |
| Scalar subquery `(SELECT ...)`                    | `scalar` crossing                           |
| `EXISTS (SELECT ...)`                             | `exists` crossing                           |
| Correlated to-one subquery / `LEFT JOIN` unnest   | `first` crossing                            |
| Correlated to-many nested result                  | `array` crossing                            |
| `VALUES (...), (...)` / literal `INSERT` rows     | `rows`                                      |
| `a op b` (`= != < <= > >=`, arithmetic, `AND/OR`) | `binary`                                    |
| `NOT`, `IS [NOT] NULL`, unary minus               | `unary`                                     |
| `CAST(x AS type)`                                 | `cast`                                      |
| `COUNT/SUM/AVG/MIN/MAX`                           | `AggTerm.fn`                                |
| `INSERT INTO t (...) VALUES/SELECT ...`           | PIR `create` statement                      |
| `UPDATE t SET ... WHERE ...`                      | PIR `update` statement (see below)          |
| `DELETE FROM t WHERE ...`                         | PIR `delete` statement (see below)          |

**`UPDATE`/`DELETE` need one extra step**, because PIR statements don't
carry a `SET`/predicate pair — they consume a _relation_ already shaped to
the mutation's contract (`pir.schema.yaml`): an `update`'s relation must
output the full primary key (to identify the row, unassigned) plus every
assigned column; a `delete`'s relation must output _exactly_ the primary
key. So `UPDATE tasks SET status = 'done' WHERE board_id = 'b1'` compiles
to `filter(scan tasks, board_id = 'b1')` then a `project` that spreads the
primary key and adds `status: lit('done')` as a field — the compiler always
appends that shaping `project`, it's not optional. `DELETE FROM tasks WHERE
...` is the same filter, then a `project` that spreads only the primary
key.

### Worked example

```sql
SELECT t.id, t.title, t.status
FROM tasks t
WHERE t.board_id = 'board-1' AND t.status != 'done'
ORDER BY t.priority DESC, t.created_at ASC
LIMIT 5
```

compiles to exactly the `protocol.Query` a human would hand-build today,
using the existing builders in `rad/protocol/build.go`:

```go
protocol.Query{
    Nodes: map[string]protocol.Node{
        "t":  {Kind: "scan", Table: "tasks", Scope: "t"},
        "n1": {Kind: "filter", Input: "t", Predicate: protocol.AndAll([]*protocol.Expr{
            protocol.Eq(protocol.Col("t", "board_id"), protocol.Lit("board-1")),
            protocol.Ne(protocol.Col("t", "status"), protocol.Lit("done")),
        })},
        "n2": {Kind: "order", Input: "n1", Terms: []protocol.OrderTerm{
            {Expr: *protocol.Col("t", "priority"), Desc: true},
            {Expr: *protocol.Col("t", "created_at")},
        }},
        "n3": {Kind: "slice", Input: "n2", Limit: intp(5)},
        "n4": {Kind: "project", Input: "n3", Fields: []protocol.Field{
            {As: "id", Expr: *protocol.Col("t", "id")},
            {As: "title", Expr: *protocol.Col("t", "title")},
            {As: "status", Expr: *protocol.Col("t", "status")},
        }},
    },
    Root: protocol.Root{Node: "n4", Cardinality: "many"},
}
```

That is the whole target: **the AST-to-IR layer's job is to become a
compiler whose sole backend is calls into `protocol/build.go` and
`protocol.Node{}` literals** — no hand-rolled JSON, no second wire
encoding. Node ids can be synthesized (`n1`, `n2`, ...); nothing about LIR
requires them to be meaningful.

## Proof-of-concept scope

**In scope for v1:**

- `SELECT` with: column refs, `t.*`/`*`, literals, `+ - * /`, `CAST`,
  `COUNT/SUM/AVG/MIN/MAX`, aliases (`AS`).
- `FROM` with one or more tables, `[INNER] JOIN ... ON`, `LEFT [OUTER]
JOIN ... ON` — matching LIR's `inner`/`left` exactly. No `RIGHT`/`FULL`/
  `CROSS`/lateral/`NATURAL` joins.
- `WHERE`: `= != <> < <= > >=`, `AND OR NOT`, `IS [NOT] NULL`,
  parenthesization, literal-list `IN (1, 2, 3)` (lowered to an `OR` chain
  of equality — no subquery `IN`, see Risks).
- `GROUP BY` / `HAVING`.
- `ORDER BY` (multiple terms, `ASC`/`DESC`).
- `LIMIT` / `OFFSET`.
- Scalar subqueries (`scalar` crossing), `EXISTS (...)` /
  `NOT EXISTS (...)` (`exists` crossing), correlated subqueries generally
  — LIR already lets a sub-relation reference an outer scope freely; the
  compiler's job is just recognizing the shape and emitting the right
  crossing, not implementing correlation itself.
- Non-recursive `WITH name AS (...)`, mapped to a LIR `binding` + `ref`.
- `INSERT INTO t (cols) VALUES (...), (...)` → `create` over a `rows`
  relation; `INSERT INTO t (cols) SELECT ...` → `create` over the compiled
  `SELECT`'s relation.
- `UPDATE t SET col = expr, ... WHERE pred` → `update`, per the shaping
  rule above. `expr` limited to literals/arithmetic/casts over the same
  row's columns in v1 — no correlated subquery on the right of `SET`.
- `DELETE FROM t WHERE pred` → `delete`, per the shaping rule above.

**Explicitly out of scope for v1** (either LIR has no target construct
yet, or it's a deliberate correctness/complexity deferral):

- `CASE`/`COALESCE`/`NULLIF`, scalar function calls beyond `CAST`, string
  functions, `LIKE`/`GLOB`/`REGEXP`, `||` concatenation — LIR's `Expr`
  union has no function-call or `case` variant yet
  (`next-steps.md`: "Parameters, CASE, COALESCE, function calls... missing
  expression capabilities").
- `IN`/`ANY`/`ALL` against a **subquery** — deferred, not merely
  unsupported: `next-steps.md` already flags that the obvious
  exists/not-exists lowering isn't null-correct under three-valued logic,
  and getting `NOT IN (subquery)` wrong on NULLs is a classic, easy-to-miss
  bug. Literal-list `IN` has no such trap (it's just an `OR` of equalities)
  and is in scope.
- Window functions; `UNION`/`INTERSECT`/`EXCEPT` (no LIR set-op node yet).
- Recursive CTEs (`tasks/1-todo/recursive-queries.md`).
- `RIGHT`/`FULL OUTER`/`CROSS`/lateral/semi/anti joins.
- Prepared-statement parameters as first-class IR (`?`, `:name`). LIR has
  no `param` node yet (`schema-flexibility.md` wants one, not built). v1
  substitutes bound values as literals at compile time — meaning no plan
  reuse across calls with different arguments, acceptable for an
  experimentation/battle-testing tool, revisit if reuse turns out to
  matter.
- Schema DDL (`CREATE TABLE`, `ALTER TABLE`) via SQL — schema management
  stays through Rad's own catalog/migrate path.
- Multi-statement scripts fused into one atomic `Program`. v1 compiles one
  SQL statement to one `Program`; a later "script mode" could map a
  semicolon-delimited block to one `Statement` per `Program.Statements` in
  order (a natural fit, since PIR already _is_ an ordered list of
  statements in one transaction), but that changes what atomicity a script
  gets and deserves its own decision, not a default.

## Architecture

```
SQL text
  │  parse (chosen library, see below)
  ▼
AST
  │  bind: resolve table/alias/column refs and infer expression types
  │  against the schema (protocol.TableInfo, from Client.Tables or
  │  schema.Parse) — a small, purely client-side analogue of what
  │  rad/engine/04_planner/bind.go does server-side, much simpler because
  │  the AST is already far more constrained than a raw graph
  ▼
Bound AST
  │  emit: walk in SQL's clause order, calling rad/protocol/build.go
  │  helpers and constructing protocol.Node values directly
  ▼
protocol.Query / protocol.Program
  │  (caller's choice: execute in-process, or protocol.MarshalQuery /
  │  MarshalProgram + POST to a real server)
```

Proposed package: `rad/sql/`, mirroring the existing `rad/protocol`,
`rad/client` siblings. Internal shape:

- `rad/sql/parse` — the chosen parser, vendored/adapted or wrapped,
  producing (or being adapted to produce) the AST the next stage consumes.
- `rad/sql/bind` — schema-aware name/type resolution over that AST.
- `rad/sql/compile` — bound-AST → `protocol.Query`/`protocol.Program`.
- `rad/sql` (root) — the public entrypoints:
  `CompileQuery(schema []protocol.TableInfo, sql string) (protocol.Query, error)`
  and `CompileProgram(schema []protocol.TableInfo, sql string) (protocol.Program, error)`.

## Parser candidates

Evaluated for: real SQLite-grammar support (not MySQL/Postgres-flavored
generic SQL wearing a SQLite hat), active maintenance, a typed/walkable Go
AST meant for external consumption (not an internal-only representation),
permissive license, and hand-written vs. generated parsing (affects how
easily we can extend it for anything it doesn't already cover).

| Library                                           | SQLite-targeted?                                       | Parser type                                        | Maintenance                                                                                                                | AST                                                                               | License                  |
| ------------------------------------------------- | ------------------------------------------------------ | -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- | ------------------------ |
| **`rqlite/sql`**                                  | Yes — full SQLite grammar bar `ATTACH`/`DETACH`        | Hand-written recursive-descent                     | Active; the exact pinned dependency of rqlite (17.6k★) itself                                                              | Comprehensive typed AST (60+ node kinds), `Visitor`/`Walk` built for external use | MIT                      |
| `antlr/grammars-v4` SQLite grammar                | Yes — canonical community grammar                      | ANTLR4-generated                                   | Grammar actively edited; ready-made Go wrappers around it are dead (`libsql/sqlite-antlr4-parser` explicitly discontinued) | Generic ANTLR parse-tree/context objects — functional, not a curated AST          | Grammar MIT, runtime BSD |
| `vitess.io/vitess/go/vt/sqlparser`                | No — MySQL                                             | goyacc-generated                                   | Very active (21.1k★)                                                                                                       | Excellent AST, wrong dialect                                                      | Apache-2.0               |
| `pingcap/tidb/pkg/parser`                         | No — MySQL                                             | goyacc-generated                                   | Very active (40.3k★)                                                                                                       | Well-documented AST, wrong dialect                                                | Apache-2.0               |
| `cockroachdb/cockroachdb-parser`                  | No — Postgres                                          | goyacc-generated                                   | Active, low adoption                                                                                                       | Good AST, wrong dialect                                                           | Apache-2.0               |
| `auxten/postgresql-parser`                        | No — Postgres                                          | goyacc-generated                                   | Moderate                                                                                                                   | Good AST, wrong dialect                                                           | Apache-2.0               |
| `xwb1989/sqlparser`, `blastrain/vitess-sqlparser` | No — MySQL-ish                                         | goyacc-generated                                   | Abandoned (2022)                                                                                                           | Stale                                                                             | Apache-2.0               |
| `modernc.org/sqlite`                              | Yes, internally — full engine, not a standalone parser | SQLite's own C `parse.c` transpiled to Go via ccgo | Active as a DB driver                                                                                                      | Machine-transpiled, not a consumable AST — not extractable                        | BSD-3                    |

**Decision: vendor `rqlite/sql`**, forking the handful of parser/scanner/AST
files into an internal package rather than importing it live. It is the
only candidate that is simultaneously SQLite-grammar-specific,
permissively licensed, hand-written (readable and extensible without an
ANTLR/Java toolchain in the loop), has a typed AST with a visitor pattern
explicitly designed for external consumption, and is proven in production
as the parser of a real distributed-SQLite system (rqlite pins this exact
package). Its grammar covers more than this POC needs (CTEs, triggers,
views, window functions, virtual tables) — that's a feature here: parse
permissively, and let the AST-to-IR mapping layer explicitly reject
whatever falls outside this POC's scope, rather than needing to patch
grammar gaps to even parse a valid query. Vendor rather than depend live:
it's a small surface, upstream is a niche side project of a small team,
and pruning/extending the grammar for our own mapping needs is easier
without waiting on upstream review. Keep the LICENSE/attribution (Ben
Johnson / rqlite) in the vendored copy.

**Fallback: the `antlr/grammars-v4` SQLite grammar**, regenerated in-house
via `antlr4-go/antlr`. The most rigorously community-vetted SQLite
grammar, with a real (if shallow) Go production consumer
(`tursodatabase/libsql-client-go` vendors a generated copy for lightweight
statement classification). Worth keeping in reserve specifically for when
recursive CTEs are tackled later (`tasks/1-todo/recursive-queries.md`) —
ANTLR's left-recursion handling is generally more robust than a
hand-rolled recursive-descent grammar for that — but its parse-tree/
context output is meaningfully clunkier to consume than `rqlite/sql`'s
purpose-built AST for everything in this POC's scope, and the ready-made
Go wrappers around it are dead ends we'd be reviving, not adopting.

Everything MySQL/Postgres-flavored (vitess, tidb, cockroachdb-parser,
auxten/postgresql-parser, xwb1989, blastrain) is well-engineered and
permissively licensed, but wrong grammar shape — SQLite-only syntax
(`PRAGMA`, `INSERT OR REPLACE`, rowid semantics, `WITHOUT ROWID`) would
need backfilling and MySQL/Postgres-only syntax stripped, a worse trade
than starting from a parser that's already SQLite-native.

**Open uncertainty, carried over from research:** no published
test-coverage figure for `rqlite/sql` surfaced (its ~200KB test file is a
size proxy, not a verified coverage number), and neither `rqlite/sql` nor
the grammars-v4 SQLite grammar appear to have been fuzzed or run against
SQLite's own reference test corpus (TH3/testfixture) — treat both as
"well-tested by hand-written unit tests," not "validated against SQLite's
own suite." Worth a differential smoke-test against real SQLite early
(see Verification) specifically to catch parser-level gaps, not just
mapping-level ones.

## Implementation plan

1. **Vendor `rqlite/sql`.** Fork the parser/scanner/AST files into
   `rad/sql/parse`, keep attribution, strip nothing yet — parse
   permissively, reject out-of-scope constructs later in the mapping
   layer, not in the grammar.
2. **Smoke-test the parser alone**, no IR yet: feed it a batch of
   real-world SQLite queries (pull from an existing open-source project's
   fixtures, not just this ADR's hand-picked examples) and confirm it
   parses the POC's target subset cleanly. This is the step that finds
   `rqlite/sql`'s actual edge cases (Risks) before anything is built on
   top of it.
3. **Build `rad/sql/bind`**: given `[]protocol.TableInfo`, resolve every
   table/alias/column reference in the AST, reject unknowns and
   ambiguities, and attach a scalar type to every expression node. Small
   by design — the AST is already far more constrained than a raw
   client-submitted LIR graph, so this is not a rebuild of
   `rad/engine/04_planner/bind.go`, just its client-side, schema-only
   sliver.
4. **Build `rad/sql/compile`**: bound AST → `protocol.Query`/
   `protocol.Program`, one node per SQL clause per the mapping table
   above, calling straight into `rad/protocol/build.go`. Land `SELECT`
   first (scan/filter/project/order/slice/join/aggregate), then crossings
   (scalar/exists/first/array), then non-recursive `WITH`, then
   `INSERT`/`UPDATE`/`DELETE` last, since they reuse the same `SELECT`
   compiler for their relation and only add the shaping `project` PIR
   needs.
5. **Wire the public entrypoints** (`CompileQuery`/`CompileProgram`) and
   exercise them against a `harness`-backed test DB — no server round
   trip needed to validate correctness at this stage.
6. **Differential-test against real SQLite** (see Verification) — the
   actual proof this is worth anything beyond "it parses."
7. **(Phase 2, once 1-6 land)** Swap `rad/sql/parse` to vendor
   `pgplex/pgparser` instead of `rqlite/sql`, re-verify `bind`/`compile`
   against it (Postgres grammar differences will surface real gaps), then
   build the `psql-wire`-based server frontend around the same compiler —
   see "Alternative: Postgres dialect and wire protocol."

## Verification: a real oracle, not just planner tests

The point of this project is external correctness pressure, so the POC's
test plan should not just be "the compiler's own unit tests pass." Plan:

1. Reuse the `tests/e2e` fixture shape (`rad.schema.yaml` + `seed.json`) as the
   shared ground truth: load the same schema and seed data into both a
   Rad instance and a real SQLite database (`modernc.org/sqlite`, pure Go,
   no cgo — keeps this dependency-light regardless of what wins as the
   parser).
2. For each hand-picked SQL query/statement in the POC's scope, run it
   against both, compiled-to-IR-then-executed on Rad and directly on
   SQLite, and diff the result sets.
3. Only after that differential harness exists does the "point an
   existing project's test suite at Rad" idea become well-founded — it's
   the stretch goal, explicitly not part of the POC itself, gated on the
   POC's scope actually covering enough of that project's query shapes to
   be worth trying.

## Alternative: Postgres dialect and wire protocol

Raised after the initial pick: instead of (or in addition to) a SQLite-text
compiler, target Postgres — both a Postgres-grammar parser and, further,
a real Postgres **wire-protocol** frontend, so unmodified Postgres clients
(`psql`, `pgx`, any ORM's Postgres driver) connect straight to Rad. This
would deliver Goal #2 (battle-testing with a real project's own traffic or
test suite) far more directly than the SQLite path: nobody's existing test
suite calls a bespoke `CompileQuery(sql string)` function, but plenty of
them already run against a Postgres connection string.

### Parser: `pgplex/pgparser`

A real, very new (created 2026-01-30, v0.2.0) project: a direct `goyacc`
port of **PostgreSQL's own grammar** (`gram.y`, targeting PG 17.7) with an
AST mirroring PG's real `parsenodes.h` structs — not a reimplementation of
"PG-like SQL," the actual grammar. It claims **99.6% pass validated
against PostgreSQL's own regression suite** (~45,000 statements), a
materially stronger fidelity signal than either Postgres-flavored parser
already in the comparison table above:

|                   | `pgplex/pgparser`                 | `cockroachdb-parser`       | `auxten/postgresql-parser` |
| ----------------- | --------------------------------- | -------------------------- | -------------------------- |
| Basis             | goyacc port of real PG `gram.y`   | goyacc, CRDB's own dialect | goyacc, CRDB's own dialect |
| Validated against | PG's own regression suite (99.6%) | CRDB's own tests           | CRDB's own tests           |
| Stars / age       | 26 / ~5.5 months                  | 44 / active                | 313 / dead since 2022      |
| License           | Apache-2.0 + PostgreSQL License   | Apache-2.0                 | Apache-2.0                 |
| Production use    | none external yet                 | low                        | Atlas, Bytebase (current)  |

It's a spinoff of Bytebase's internal tooling (contributors include
Bytebase's founder), built as a higher-fidelity successor to the
`auxten/postgresql-parser` Bytebase currently ships — a real signal, not a
hobby project, despite the young repo. Weaknesses: thin docs (one entry
point, `parser.Parse(sql)`, no documented visitor/walker API, unlike
`cockroachdb-parser`'s years-old walk helpers), zero external
issues/PRs/battle-testing yet, pre-1.0 API-churn risk.

**If Rad's ambition here is real Postgres-dialect fidelity** (which the
wire-protocol idea below implies), `pgplex/pgparser` is the architecturally
correct bet — CRDB's dialect diverges from and is incomplete relative to
real Postgres grammar, so parsing "CRDB SQL" is parsing an approximation
of an approximation. `auxten/postgresql-parser` remains the
safer-if-stability-matters-more choice (five years of real usage, if dead
since 2022); for Rad's currently tiny target subset either parses fine —
the difference only bites at the edges, later.

### Wire protocol: feasible, on top of `psql-wire`

`github.com/jeroenrinzema/psql-wire` is purpose-built for exactly this:
"bring your own SQL parsing/execution, we handle the wire protocol."
Actively maintained (44 releases, latest ~April 2026), permissive license,
handles session lifecycle, simple **and** extended query protocol state
machines, pluggable auth, SSL, and type-tagged columns. (`jackc/pgx/v5/
pgproto3` is the lower-level alternative — pure message framing, no
session/type/auth layer — psql-wire is built on the same idea one level
higher, and is the right altitude to start from.) CockroachDB and
RisingWave both wrote their own protocol layer from scratch, but both
predate/exceed what a library like this targets; for a side-project-scale
database, psql-wire is the intended shortcut.

**The one scope trap to not underestimate: simple query protocol is not
enough.** Confirmed across `node-postgres`, JDBC, `psycopg3`, and Npgsql —
real ORM traffic is parameterized as `Parse → Bind → Execute → Sync`
(extended protocol, typically an unnamed statement/portal), not simple-
protocol text. Skipping extended protocol means only `psql` and legacy
simple-only clients work, which quietly defeats the entire point ("point
real ORMs at Rad").

**What genuinely is skippable for a v1**, keeping the undertaking to a
"few weeks, not months" scale: SSL/TLS (drivers fall back to plaintext
when the server answers the SSLRequest with `N`), SCRAM/MD5 auth
(trust/cleartext is fine for a local dev/test tool), named prepared-
statement caching (the unnamed statement/portal path suffices), the COPY
protocol, and binary wire format (text-format results are accepted by
virtually every driver even when binary is preferred). Type-OID mapping
for Rad's four scalars (`text`/`int8`/`float8`/`bool`) is trivial.

### What stays the same either way

The AST-to-IR mapping layer (`rad/sql/bind`, `rad/sql/compile`) is
transport- and largely dialect-agnostic — it consumes _a_ parsed AST and
emits `protocol.Query`/`protocol.Program` via `rad/protocol/build.go`
regardless of whether that AST came from `rqlite/sql` or `pgplex/pgparser`,
and regardless of whether the SQL text arrived via a bare function call or
a wire-protocol executor callback. Only `rad/sql/parse` (which parser) and
the transport (a `CompileQuery` call vs. a `psql-wire` server loop) differ.
The mapping table, the `UPDATE`/`DELETE` shaping rule, and the in/out-of-
scope list above (no `CASE`, no subquery `IN`, no windows, no recursive
CTEs) all carry over unchanged to a Postgres dialect — Postgres just has
more syntax around the same missing-LIR-primitive edges (e.g. Postgres's
richer `CASE`/window/array-operator surface hits the same "LIR doesn't
have this yet" ceiling, just with more syntax pointing at it).

### Sequencing: both, staged — but not set in stone

This whole project is a POC/prototype: the point is proving the
implementation works, not locking down an architecture around it.
Sequencing, loosely: phase 1 ships the SQLite/`rqlite/sql` text-compile
POC first — it's cheap, and it's what proves the AST→IR mapping layer
(`rad/sql/bind`, `rad/sql/compile`) actually works before any
protocol-level investment. Phase 2 is Postgres dialect (`pgplex/pgparser`)
plus a `psql-wire` frontend, reusing phase 1's `bind`/`compile` layers
essentially unchanged (see "What stays the same either way" above).

What phase 2 actually _is_ — embedded into Rad's own server process, or a
separate standalone service/proxy sitting in front of Rad that speaks
Postgres wire protocol on one side and Rad's existing client/HTTP API on
the other — is explicitly undecided, and not worth deciding yet. A
separate front-facing layer is the more likely shape (keeps Rad's own
server untouched, the wire-protocol piece can be thrown away or rewritten
freely), but that's a guess, not a commitment. Revisit once phase 1 exists
and phase 2 is actually being built — the implementation is what matters
right now.

## Risks / open questions

- **LIR's own gaps set the frontend's ceiling.** The frontend can't emit
  what LIR can't express (`CASE`, function calls, `UNION`, `param`,
  windows). If this project proves valuable, that's a concrete forcing
  function to prioritize those items in `next-steps.md`'s roadmap — the
  frontend surfaces demand for them rather than us guessing.
  - `next-steps.md` "Windows and recursive queries" — reaffirmed here as
    still out of scope.
- **Type affinity/looseness is the main limiter on the "redirect a real
  test suite" stretch goal, on either dialect.** Anything relying on
  SQLite's loose column typing, or Postgres-specific types Rad has no
  equivalent for (`jsonb`, arrays, enums, `timestamptz`, etc.), simply
  won't compile against Rad's strict four-scalar schema — expect this to
  cut hard into which existing test suites/ORMs are viable candidates at
  all, before SQL syntax coverage is even the bottleneck. This risk is
  larger, not smaller, on the Postgres path, since Postgres's richer type
  system is a bigger part of why people reach for it over SQLite in the
  first place.
- **Three-valued-logic parity** needs checking construct-by-construct, not
  assumed: SQLite's `NULL` handling in aggregates/comparisons is mostly
  ANSI-standard, but each mapped construct (`SUM`/`AVG` over an all-NULL
  group, `x = NULL`, `NOT IN` if ever added) needs an explicit check
  against LIR's stated K3 semantics rather than an assumption they match.
- **Parser risk is now scoped, not eliminated.** `rqlite/sql` has no
  confirmed coverage figure and no known fuzzing/reference-suite history
  (see Parser candidates) — vendoring it is the right bet, but the POC's
  first real milestone should be feeding it a batch of real-world SQLite
  queries (not just our own hand-picked POC-scope examples) to find its
  actual edge cases before building the mapping layer on top of an
  unproven assumption.

## Related

- `tasks/1-todo/recursive-queries.md` — recursive CTEs, deliberately
  excluded here, worked as its own follow-on design.
- `tasks/1-todo/next-steps.md` — the SQL-comparability roadmap table this
  frontend's scope is measured against, and the source of the CASE/
  function-call/param/union gaps noted above.
- `tasks/3-done/relation-bindings.md` — the `bindings`/`ref` machinery
  non-recursive `WITH` rides on.
- `tasks/1-todo/schema-flexibility.md` — the `param` node and future
  `union`/window sketches this frontend will eventually want.
- `tasks/3-done/data-mutation-and-transaction-protocol.md` — PIR itself,
  whose statement shapes (`update`/`delete`'s relation contract
  especially) this ADR leans on directly.
