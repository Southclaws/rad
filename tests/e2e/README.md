# tests/e2e

Fixture-driven end-to-end tests. Each subdirectory is one self-contained
scenario: a schema, seed data, and a program to run against it with
assertions on the result. `e2e_test.go` is the whole runner — it discovers
every fixture directory, gives each a fresh in-memory database, and runs it
in parallel through the real client → server → bind → plan → execute path.
A fixture is data, not code, so the suite grows by adding directories, never
by editing the runner:

```
go test ./tests/e2e/            # run every fixture
go test ./tests/e2e/ -run E2E/create_task   # one fixture
```

## Layout

```
tests/e2e/<test-name>/
  schema.rad            # this test's schema — only the tables it needs
  seed.json              # rows to insert before the program runs
  test_<test-name>.json  # the program to run, its expected result, and assertions
```

## `schema.rad`

A minimal schema scoped to the one scenario — not the full demo schema.
Keeps each fixture self-contained and fast, and means a fixture never breaks
because an unrelated table changed shape. Same YAML format `rad migrate`
consumes (see `examples/demo/schema.rad` for the full-featured reference).

Omit `default: uuid()` / `default: now_ms()` on any column whose value feeds
into `result` or an assertion's `expect` — see Determinism below.

## `seed.json`

An ordered array of `{table, rows}` groups, inserted in array order:

```json
[
  { "table": "teams", "rows": [{ "id": "team-eng", "name": "Engineering" }] },
  { "table": "boards", "rows": [{ "id": "board-launch", "team_id": "team-eng", "name": "Launch" }] }
]
```

An array, not a table-keyed object, so insertion order — and therefore FK
dependency order — is unambiguous in JSON (object key order isn't a format
guarantee). Each row is inserted the same way an application's writes would
be (through `Create`), so values coerce exactly as production data does.

## `test_<test-name>.json`

```json
{
  "program": {
    "statements": [
      {
        "name": "insert_task",
        "kind": "create",
        "table": "tasks",
        "relation": {
          "nodes": { "r": { "kind": "rows", "scope": "r", "columns": [ ... ], "rows": [ [ ... ] ] } },
          "root": { "node": "r", "cardinality": "many" }
        }
      }
    ]
  },
  "result": [ { ... } ],
  "assertions": [
    { "name": "...", "query": { "nodes": { ... }, "root": { "node": "...", "cardinality": "many" } }, "expect": [ ... ] }
  ]
}
```

- **`program`** — a real PIR `Program`: `{"statements": [...], "result"?}`,
  wire-format JSON exactly as `protocol.MarshalProgram` emits. Each
  statement is `{name, kind, table?, relation}` — `kind` is one of `query`
  / `create` / `update` / `delete`; `table` is set for the three mutation
  kinds; `relation` is a full LIR `Query`. A literal insert's `relation` is
  a one-node `rows` relation (declared `columns` + positional `rows`
  arrays) — see `tests/harness/harness.go`'s `DB.Insert` for the reference
  construction. `result` names which statement's output the program
  returns; it may be omitted only when there's exactly one statement, in
  which case that statement is the result. This is executed via the
  client's single mutation entry point, `Client.Execute(ctx, program)` —
  there is no separate insert/update/delete call, and no session-based
  transaction type; atomicity comes from a program having multiple
  statements, not from a held transaction.
- **`result`** — the exact value `ProgramResult.Result` should produce:
  shaped like a query result (array/object/scalar/null) according to the
  cardinality of the result statement's LIR root. A `create` statement
  built on a `cardinality: "many"` `rows` relation returns an array of the
  created rows.
- **`assertions`** — a list of `{name, query, expect}`. `query` is a full
  LIR query, wire-format JSON exactly as `protocol.MarshalQuery` emits
  (`nodes` + `root`) — no shorthand, so what's sent is what's visible in the
  file. `expect` is a JSON array when the query's root `cardinality` is
  `"many"` (compared as an exact ordered row list), or a single JSON object
  for `"first"` / `"exactly_one"`, or a naked value for `"scalar"`.
- **`error`** — instead of `result`/`assertions`, a fixture may assert the
  program *fails* — the negative path (malformed programs, constraint
  violations, mutation misses). Two forms:
  - a bare string — a substring the problem *detail* must contain (shorthand
    for `{"contains": "…"}`); or
  - an object `{code?, reason?, contains?}` — `code` pins the wire problem
    class (`invalid` · `execution_failed` · `not_found` · `conflict` ·
    `internal`), `reason` pins the stable fine-grained reason
    (`unknown_table`, `mutation_target_not_found`, …), and `contains` pins a
    detail substring. Any field omitted is not asserted.

  Asserting `reason` is what the conformance suite pins down: the five codes
  are coarse, but the reason names exactly which check fired. When `error` is
  set the program's `result` is not diffed (a failed program has none), but
  any `assertions` **still run** against the post-failure state — which is how
  a fixture verifies that a failing program left the store untouched (atomic
  rollback).

Comparison is by canonical JSON (object keys sorted, numbers as
`json.Number`), so key order never matters and an int64 column's `1000`
matches the fixture's `1000` exactly.

### A caveat on pinning `result` for mutations

A mutation statement's result is the affected rows as a *bag* — its order
is unspecified. Only pin `result` for a mutation when the answer is
order-independent: a single created/updated/deleted row, or an empty
result. For multi-row mutation state, pin the outcome with an `assertions`
query that carries an explicit `order` instead (see `delete_by_query`).
Query statements shaped `many` already carry an order, so their `result`
and `expect` are compared as an exact sequence.

## Determinism

Every value that flows into `result` or an assertion's `expect` must be
explicit in `seed.json` or `program`, not left to a generated default —
`uuid()` ids and `now_ms()` timestamps are non-reproducible, so schemas in
this directory should supply ids and timestamps as plain literals instead
(e.g. `"team-eng"`, `1000`) rather than declaring `default: uuid()` /
`default: now_ms()` on columns the fixture asserts against.

## Adding a fixture

1. Write `schema.rad` scoped to just what the scenario needs.
2. Write `seed.json` — the pre-existing state, in FK-safe insertion order.
3. Write `test_<name>.json` — the program, its expected result, and enough
   assertions to pin down the state it left behind (or an `error` for a
   negative case).
4. `go test ./tests/e2e/ -run E2E/<name>` — the runner picks the directory
   up automatically. No Go to write.

Exactly one `test_*.json` per directory; a directory without one is
skipped (so a bare `schema.rad`/`seed.json` in progress won't fail the
suite).
