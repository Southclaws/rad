# Design: Aggregations

Status: proposed (not yet implemented)

Basics only: `count`, `sum`, `avg`, `min`, `max`. No `GROUP BY`, no
`HAVING`, no `DISTINCT`, no expressions inside aggregates, no window
functions — those stay out of scope per the V0 spec until a demo scenario
demands them.

## Motivation, demo-first

The Tracker demo wants exactly two things today:

1. Board cards showing task counts without fetching the tasks
   (`open: 4 · done: 9`).
2. "Stats" lines: average estimate, earliest due date, per assignee.

Both are also the cheapest HN criticism of the current system ("a task
tracker that can't count tasks"). The design below covers them and nothing
speculative.

## Shape

Two forms, mirroring how reads already work:

### 1. Root aggregation — a sibling of `Read`

An aggregate query is *not* a read that returns rows; it returns one record
of scalars. Keep it a distinct QIR node and endpoint rather than a mode
flag on `Read`:

```go
// 03_lir
type AggFn string // count | sum | avg | min | max

type AggTerm struct {
    Fn     AggFn
    Column string // empty for count (row count)
    As     string
}

type Aggregate struct {
    Table  string
    Filter Expr      // same expression language as Read
    Aggs   []AggTerm // at least one
}
```

Wire (`POST /aggregate`):

```json
{
  "table": "tasks",
  "filter": {"op": "eq", "column": "board_id", "value": "b1"},
  "aggs": [
    {"fn": "count", "as": "total"},
    {"fn": "avg", "column": "estimate", "as": "avg_estimate"},
    {"fn": "max", "column": "due_at", "as": "latest_due"}
  ]
}
→ {"result": {"total": 13, "avg_estimate": 2.75, "latest_due": 1783600000000}}
```

### 2. Aggregate includes — scalars inside a nested read

An `Include` whose `aggs` is set returns an object of scalars instead of a
record array, under its `as` name — the "board with task counts" case in
one round trip:

```json
{
  "table": "boards",
  "include": [
    {"fk": "tasks_board_id_fk", "dir": "children", "as": "open_tasks",
     "filter": {"op": "eq", "column": "done", "value": false},
     "aggs": [{"fn": "count", "as": "n"}]}
  ]
}
→ {"id": "b1", "name": "Launch", "open_tasks": {"n": 4}}
```

`aggs` is mutually exclusive with `include`/`order_by`/`limit` on the same
include (validated by the planner). Parent-direction includes cannot
aggregate (they are at most one row).

## Semantics

SQL-standard where SQL is unambiguous:

- `count` with no column counts rows; `count(col)` counts non-NULL values.
- `sum`/`avg`/`min`/`max` skip NULLs entirely.
- Typing: `count` → int64; `sum` → the column's type (int64 or float64);
  `avg` → float64 always; `min`/`max` → the column's type (min/max on text
  is lexicographic; on bool, false < true).
- Empty input: `count` → 0; everything else → NULL. (`avg` of nothing is
  NULL, not 0 — the classic trap.)
- `sum`/`avg` require a numeric column; validated at plan time. Overflow is
  not detected (POC).

## Planning & execution

Nothing new below the IR:

- **Planner** (`04_planner`): `PlanAggregate` reuses `chooseAccess` — the
  filter's equality prefix picks PK lookup / index scan / full scan exactly
  as for reads. It validates columns, types, and the include exclusivity
  rules, and resolves `As` collisions.
- **Executor** (`05_exec`): a fold, not a materialization. Stream rows from
  the access path, apply the residual filter, accumulate
  `{count, sum, min, max}` per term; `avg = sum/count` at the end. Never
  builds a row slice — this is the first operator that is strictly cheaper
  than the read it replaces.
- **Aggregate includes**: `fetchChildren` already locates child rows per
  parent (FK index or scan); with `aggs` set it folds them instead of
  recursing into `attachIncludes`. Same N+1 shape as record includes — when
  includes get batched, aggregate includes come along for free.

## Surfaces

- **Server**: `POST /aggregate` (+ `/tx/{id}/aggregate`), and `aggs` on
  include objects inside `/query`.
- **Go runtime** (`radclient`): `Aggregate(ctx, protocol.Aggregate) (protocol.Record, error)`.
- **Generated Go** — sugar on the query builder, generated per column with
  type-appropriate methods:

  ```go
  n, err := db.Tasks.Query().BoardIDEq(id).StatusNe("done").Count(ctx)      // int64
  avg, err := db.Tasks.Query().BoardIDEq(id).AvgEstimate(ctx)               // *float64 (nil = no rows)
  latest, err := db.Tasks.Query().MaxDueAt(ctx)                             // *int64
  ```

  `Count` on every builder; `Sum{Col}`/`Avg{Col}` for numeric columns;
  `Min{Col}`/`Max{Col}` for every comparable column. Include builders gain
  `Count()` (yielding e.g. `Board.OpenTasksCount *int64`) as the first
  aggregate-include surface.

- **Generated TS**: `await db.tasks.query().boardIdEq(id).count()`,
  `.avgEstimate()`, etc. — same derivation rules.

## Non-goals (this iteration)

`GROUP BY` (the natural next step — it changes the result shape to rows and
deserves its own design), `HAVING`, `DISTINCT`, multi-aggregate builder
calls in generated code (compose via the runtime's `Aggregate` if needed),
and any pushdown cleverness (index-only counts) — correctness first, the
fold is already O(matching rows).

## Order of work

1. `lir.Aggregate` + planner validation + executor fold, with layer tests.
2. Wire types + `/aggregate` endpoint + runtime methods (Go, TS).
3. Codegen sugar (Go + TS) + demo: board cards with open/done counts and an
   average-estimate stat line — the demo change that justifies the feature.
