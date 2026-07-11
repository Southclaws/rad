# Design: Aggregations

Status: implemented (count/sum/avg/min/max); superseded in structure by
docs/design/lir-v2.md — the unification this document deferred ("where this
wants to go") was built: `Read`/`Include` dissolved into the relation graph,
aggregation became the `aggregate` node (grouped or global), and folded
relations cross into projections via `first`/`scalar`. The semantics below
(NULL-skipping, empty-set rules, typing) carried over unchanged.

Basics only: `count`, `sum`, `avg`, `min`, `max`. No `GROUP BY`, no
`HAVING`, no `DISTINCT`, no expressions inside aggregates, no window
functions — those stay out of scope per the V0 spec until a demo scenario
demands them.

## Motivation, demo-first

The Tracker demo wants exactly two things today:

1. Board cards showing task counts without fetching the tasks
   (`3 tasks · 3 open · 0 done`).
2. Stats lines: average estimate, the next deadline.

Both are also the cheapest criticism of the current system ("a task tracker
that can't count tasks"). The design below covers them and nothing
speculative.

## Shape: a fold is a shape, not an operation

The first draft made `Aggregate` a *sibling* of `Read` — a distinct LIR node
with its own `/aggregate` endpoint. That was SQL thinking in disguise
(`SELECT ...` vs `SELECT count(*) ...` wearing two struct names), and it
would have leaked an internal taxonomy into the public API.

The child side of the IR already had the better instinct: an `Include` is
"materialise this related thing," and its `Dir` (`parent` → one, `children`
→ many) is a *cardinality*, not a verb. The parent doesn't care what the
child resolves to. Aggregation is just a third cardinality of that same idea:
fold to one object of scalars.

So there is no `Aggregate` node. Aggregation is a single `Aggs` field that
appears **symmetrically** on the two relation-materialising IR nodes:

```go
// 03_lir
type AggFn string // count | sum | avg | min | max

type AggTerm struct {
    Fn     AggFn
    Column string // empty only for count() over rows
    As     string
}

type Read struct {
    Table   string
    Filter  Expr
    // ... OrderBy, Offset, Limit, Include ...
    Aggs    []AggTerm // present → fold the matching rows to one scalar object
}

type Include struct {
    // ... FK, Dir, As, Filter, OrderBy, Limit, Include ...
    Aggs    []AggTerm // present → fold the matching children to one scalar object
}
```

Present, the relation folds to scalars; absent, it yields records — the same
switch `Include.Dir` already makes. `Aggs` is a shape annotation on a
relation; it deliberately never becomes another arm of the `Expr` AST (this
is not "JSON SQL").

### Wire — one query operation

Aggregation rides the existing `POST /query`. `protocol.Read` and
`protocol.Include` each gain an optional `aggs`; asking for a fold is the
same kind of request as asking for an include. There is no `/aggregate`
endpoint and no `Aggregate` verb.

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
→ {"records": [{"total": 13, "avg_estimate": 2.75, "latest_due": 1783600000000}]}
```

A root aggregate comes back as a single scalar record. An aggregate include
comes back as a scalar object under its `as` name:

```json
{
  "table": "boards",
  "include": [
    {"fk": "tasks_board_id_fk", "dir": "children", "as": "open_tasks",
     "filter": {"op": "eq", "column": "done", "value": false},
     "aggs": [{"fn": "count", "as": "n"}]}
  ]
}
→ {"records": [{"id": "b1", "name": "Launch", "open_tasks": {"n": 4}}]}
```

On an include, `aggs` is mutually exclusive with `include`/`order_by`/`limit`
(a fold covers the whole matching set), and rejected on parent-direction
includes — a parent relation's cardinality is zero-or-one, so there is
nothing useful to fold.

## Semantics

SQL-standard where SQL is unambiguous:

- `count` with no column counts rows; `count(col)` counts non-NULL values.
- `sum`/`avg`/`min`/`max` skip NULLs entirely.
- Typing: `count` → int64; `sum` → the column's numeric type (`sum(int64)`
  is int64, `sum(float64)` is float64); `avg` → float64 always; `min`/`max`
  → the column's type (min/max on text is lexicographic; on bool, false <
  true).
- Empty input: `count` → 0 (never NULL); everything else → NULL. (`avg` of
  nothing is NULL, not 0 — the classic trap.)
- `sum`/`avg` require a numeric column and `avg` requires a column at all;
  both validated at plan time. Overflow is not detected (POC).

## Planning & execution

Nothing new below the IR:

- **Planner** (`04_planner`): aggregate planning is a validation pass, not a
  new plan shape. A folded read reuses `chooseAccess` — the filter's equality
  prefix picks PK lookup / index scan / full scan exactly as for records.
  `validateAggs` checks columns, numeric-ness for sum/avg, and distinct `As`.
  Combining `aggs` with `order_by`/`limit`/`include` is rejected with a
  friendly, human-worded error.
- **Executor** (`05_exec`): a fold over the fetched-and-filtered rows,
  accumulating `{count, sum, min, max}` per term; `avg = sum/count` at the
  end. Root aggregates return one `Record` whose `Columns` are the scalars;
  aggregate includes fold children into `Record.Scalars`, rendered by the
  frontend as a nested object. (The fetch still materialises the row slice it
  folds — a genuinely streaming fold is a later optimisation, not a
  correctness concern.)

## Surfaces

- **Go runtime** (`radclient`): none needed — the generated code builds a
  `protocol.Read` with `Aggs` and calls the existing `Query`.
- **Generated Go** — terminal methods on the query builder, per column with
  type-appropriate signatures:

  ```go
  n, err   := db.Tasks.Query().BoardIDEq(id).StatusNe("done").Count(ctx)  // int64, never nil
  avg, err := db.Tasks.Query().BoardIDEq(id).AvgEstimate(ctx)             // *float64 (nil = no rows)
  next,err := db.Tasks.Query().BoardIDEq(id).MinDueAt(ctx)                // *int64
  ```

  `Count` on every builder (non-null `int64`); `Sum{Col}`/`Avg{Col}` for
  numeric columns; `Min{Col}`/`Max{Col}` for every column. The fold methods
  reuse the accumulated filter and drop ordering/pagination/includes.

- **Generated TS**: the same derivation — `await db.tasks.query()
  .boardIdEq(id).count()` → `Promise<number>`; `.avgEstimate()` →
  `Promise<number | null>`; `.minDueAt()`, etc.

## Where this wants to go (not built)

`Aggs`-as-a-shape is the minimal form of a bigger idea: one relation-
materialising node whose *shape* (records / grouped rows / a scalar fold) and
*cardinality* is explicit, so `Read` and `Include` stop being separate types
at all. That unification is deliberately **not** built yet — the plan is to
let it emerge from real demo pressure (a Tracker, a small forum, a Shopify-
lite orders app) rather than design it in the abstract, the way an IR is
shaped by the frontends it has to serve. `Aggs` is placed so it doesn't block
that: it's an annotation on a relation, never an expression, and `GROUP BY`
(which turns a scalar fold into grouped rows) is the obvious next shape to
grow into the same node.

Also out for this iteration: `HAVING`, `DISTINCT`, multi-aggregate builder
sugar (compose via the runtime if needed), aggregate-include *codegen* (the
wire and engine support it; the sugar that reshapes the parent model can wait
for a demo that needs it), and pushdown cleverness (index-only counts) — the
fold is already O(matching rows).
