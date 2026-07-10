# Design: QIR v2 — the relation graph

Status: approved design, implementation in progress

The query IR, the planner, and the read executor are being replaced. This
document is the normative reference for the new semantics: what the IR *is*,
what every node means, how NULL behaves, how the wire expresses a query graph,
and how the graph maps onto physical KV execution. The implementation plan
(commit sequence, file inventory) lives with the work; this document is the
part that must be agreed before engine code exists, the way
`aggregations.md` preceded aggregation.

## Motivation

The current engine contains two databases. The one that serves the wire is the
shaped read: `protocol.Read` lowers to `lir.Read`, plans to a `ShapedRead` —
a request struct with an `Access` field — and executes in one monolithic
routine where filtering, ordering, pagination, aggregation, and relationship
traversal are all field special-cases. The other, a conventional relational
algebra (`lir.Query`), sits unused. Every future feature (joins, GROUP BY,
expressions, cursor pagination) would grow `ShapedRead` another field, another
validator branch, and another executor clause — the special-case maze of a
hand-written SQL executor, with nicer JSON at the entrance.

The shaped read got the important things right: results shaped like the
application's data, named relationship traversal, logical filters separated
from physical access paths, the residual filter as the source of truth. QIR
v2 keeps all of that and fixes the architecture underneath with one idea:

> **A relationship is not a kind of query. It is an ordinary correlated
> relation, materialised into an output shape.**

Commit to that and `Include`, `Parent`, `Children`, `AggregateInclude`, and
`ShapedRead` all dissolve — they were four spellings of "a relation,
correlated to the row above it, rendered as object / array / scalar."

## The two categories

The IR has exactly two categories. There is no third "shape" category:
shaping is projection, and nesting lives in the value model.

**`Relation`** — a possibly-empty, possibly-many stream of structurally typed
rows. Operators consume relations and produce relations.

**`Expr`** — a scalar computation evaluated in some scope. Expressions may
*consume* relations, but only through an explicit cardinality-crossing
operator that states how many-becomes-one.

Every relation node obeys the same laws:

```go
type Relation interface {
    Output() RowType        // the row type this relation produces
    Inputs() []Relation     // child relations
    FreeScopes() ScopeSet   // scopes referenced but not defined beneath
    Card() Cardinality      // {Min, Max} bounds, Max may be Unbounded
}
type Expr interface {
    Type() Type             // inferred type, including nullability
    FreeScopes() ScopeSet
}
```

`FreeScopes` is the whole correlation story. There is no `Correlate` node: a
sub-relation that references a scope it does not define is *correlated*, as a
derived property. The binder computes it; the planner exploits it.

## Relation operators

```text
Scan      { table, scope }            introduce a table; bind a scope label
Filter    { input, predicate }        keep rows where predicate is TRUE (3VL)
Project   { input, fields }           establish a new row type; fields are exprs
Join      { left, right, kind, on }   inner | left  (semi/anti reserved)
Aggregate { input, groups, terms }    fold; empty groups ⇒ exactly one row
Order     { input, terms }            stable sort; NULLs first asc, last desc
Slice     { input, offset, limit }    limit 0 = unlimited (existing convention)
```

Notes:

- `Scan` is the only scope-binding node in v1. The scope label is how column
  references name their source; after binding, labels become dense scope IDs.
- `Filter` keeps only rows whose predicate evaluates to `TRUE` — not
  `UNKNOWN`. This is the load-bearing 3VL rule.
- `Project` is not column selection; it establishes a new row type. Aliases,
  computed values, and nested materialisation all live here.
- `Aggregate` with empty `groups` is the global fold: cardinality exactly 1
  (the `count → 0, sum/avg/min/max → NULL` empty-set rules are unchanged from
  v1 aggregation). With groups, it is GROUP BY: one row per distinct group.
  There is no separate "fold mode" — grouping is the same node.
- The old planner's rule "aggregates cannot combine with order/limit/include"
  disappears as a *structural* impossibility to state — order-then-aggregate
  is legal algebra. What remains is a binder rule: columns referenced above an
  `Aggregate` must resolve to its group exprs or term outputs.

## Expression operators

```text
Literal  { value }                    raw scalar; typed by the binder in context
Column   { scope, name }              a column of a bound scope
Unary    { op, expr }                 not · negate · is_null · is_not_null
Binary   { op, left, right }          eq ne lt lte gt gte · and or · add sub mul div
Call     { fn, args }                 reserved (empty registry this arc)
Cast     { expr, to }                 explicit type conversion
```

There is deliberately no special `Eq` node. Equality is `Binary{eq}` like any
other comparison; the planner's access-path analysis *extracts* searchable
constraints from the regular tree rather than the tree being shaped for the
optimizer's convenience.

### Cardinality crossings

Crossing from `Relation` into `Expr` requires stating the cardinality
conversion. These are the only four doors:

```text
Exists(rel)  : Expr<Bool>            true iff rel has ≥1 row; never NULL
First(rel)   : Expr<Row?>            first row as a nested object, or NULL
Scalar(rel)  : Expr<T?>              single-column relation → its value, or NULL
Array(rel)   : Expr<Array<Row>>      all rows as a nested array; empty, never NULL
```

`First` and `Scalar` over a multi-row relation take the first row (cheap,
deterministic given the relation's ordering); they do not error. A relation
can therefore appear almost anywhere — inside a projection field, inside a
filter predicate — but never *as* a scalar without declaring the conversion.

Shaping falls out: a projection field whose expr is `First(sub)` renders as a
nested object (or JSON null), `Array(sub)` as a nested array, `Scalar(sub)` as
a naked value, `Exists(sub)` as a boolean. The old include taxonomy is gone.

### The root

A query is a relation plus a root cardinality:

```text
Query { root: Relation, cardinality: many | first | exactly_one | scalar }
```

`many` → array of objects; `first` → object or null; `exactly_one` → object,
error otherwise; `scalar` → single value (root output must have arity 1,
enforced at bind time). The wire response stays `{"records": [...]}` in all
cases.

## Three-valued logic

Comparisons over nullable values evaluate to Kleene K3 tri-bools:

```text
eq/ne/lt/lte/gt/gte with any NULL operand  →  UNKNOWN
AND:  F∧x=F · T∧T=T · otherwise UNKNOWN
OR:   T∨x=T · F∨F=F · otherwise UNKNOWN
NOT:  ¬T=F · ¬F=T · ¬UNKNOWN=UNKNOWN
is_null / is_not_null                       →  always TRUE or FALSE
Exists(rel)                                 →  always TRUE or FALSE
Filter keeps TRUE only.
```

This deliberately changes one observable behaviour: today `NOT (x = 1)`
*matches* rows where `x` is NULL (the comparison collapses to false, NOT flips
it). Under K3 the comparison is UNKNOWN, NOT UNKNOWN is UNKNOWN, and the row
is filtered out — the SQL-standard and least-surprising reading. `is_null`
remains the only way to match NULLs.

Relatedly, unique indexes stop treating NULL as an ordinary value: an index
tuple containing a NULL component is exempt from uniqueness enforcement
(entries are still written; the check skips them). NULLs are distinct, the
SQL default.

## Correlation, worked

The child relation references a scope introduced outside it:

```text
boardTasks = Filter(
    Scan(tasks, scope: t),
    t.board_id = b.id          -- b is bound by an enclosing Scan(boards, b)
)
```

The binder sees `FreeScopes(boardTasks) = {b}` — the relation is correlated.
It appears in a projection over boards:

```text
Project(
    input: Scan(boards, scope: b),
    fields: {
        id:    b.id,
        tasks: Array(boardTasks)
    }
)
```

Semantically: for each board row, `boardTasks` evaluates with `b` bound to
that row. *Operationally* the planner is free to do anything result-equivalent
— per-row nested execution, one batched fetch partitioned by key, a join.
N+1 is no longer the semantics; it is at most an implementation fallback.

## The wire format

`POST /query` takes the graph directly. Nodes live in a flat map keyed by
caller-chosen ids; references are plain strings, so the JSON Schema has no
recursion through the map (only `Expr`→`Expr`, which the toolchain already
handles). Both unions follow the repo idiom: one flat object, a `kind` enum,
all-optional payload fields, validated server-side.

```text
Query   { nodes: { <id>: RelNode }, root: { node: <id>, cardinality } }
RelNode kind ∈ scan filter project join aggregate order slice
Expr    kind ∈ lit col unary binary call cast exists first scalar array
```

- `scan` requires `scope`, unique per query.
- `project` has `spread: [scopes]` ("all columns of these scopes first") plus
  `fields: [{as, expr}]`.
- `and`/`or` are binary; clients left-fold chains.
- Crossings (`exists|first|scalar|array`) carry `node: <id>` referencing the
  sub-relation.
- The binder enforces single-consumer (tree) usage in v1; string ids leave
  DAG sharing open without a wire change.

### The forcing query

The whole design is acceptance-tested by one query: *boards → each board's
owner → the first 20 open tasks by priority → each task's assignee and
comment count.* On the wire:

```json
{
  "nodes": {
    "boards": { "kind": "scan", "table": "boards", "scope": "b" },

    "owner":       { "kind": "scan", "table": "users", "scope": "o" },
    "owner.match": { "kind": "filter", "input": "owner",
      "predicate": { "kind": "binary", "op": "eq",
        "left":  { "kind": "col", "scope": "o", "column": "id" },
        "right": { "kind": "col", "scope": "b", "column": "owner_id" } } },

    "tasks":       { "kind": "scan", "table": "tasks", "scope": "t" },
    "tasks.match": { "kind": "filter", "input": "tasks",
      "predicate": { "kind": "binary", "op": "and",
        "left":  { "kind": "binary", "op": "eq",
          "left":  { "kind": "col", "scope": "t", "column": "board_id" },
          "right": { "kind": "col", "scope": "b", "column": "id" } },
        "right": { "kind": "binary", "op": "eq",
          "left":  { "kind": "col", "scope": "t", "column": "status" },
          "right": { "kind": "lit", "value": "open" } } } },
    "tasks.sorted": { "kind": "order", "input": "tasks.match",
      "terms": [ { "expr": { "kind": "col", "scope": "t", "column": "priority" },
                   "desc": true } ] },
    "tasks.page":   { "kind": "slice", "input": "tasks.sorted", "limit": 20 },

    "assignee":       { "kind": "scan", "table": "users", "scope": "a" },
    "assignee.match": { "kind": "filter", "input": "assignee",
      "predicate": { "kind": "binary", "op": "eq",
        "left":  { "kind": "col", "scope": "a", "column": "id" },
        "right": { "kind": "col", "scope": "t", "column": "assignee_id" } } },

    "comments":       { "kind": "scan", "table": "comments", "scope": "c" },
    "comments.match": { "kind": "filter", "input": "comments",
      "predicate": { "kind": "binary", "op": "eq",
        "left":  { "kind": "col", "scope": "c", "column": "task_id" },
        "right": { "kind": "col", "scope": "t", "column": "id" } } },
    "comments.count": { "kind": "aggregate", "input": "comments.match",
      "aggs": [ { "fn": "count", "as": "n" } ] },

    "tasks.shaped": { "kind": "project", "input": "tasks.page", "spread": ["t"],
      "fields": [
        { "as": "assignee",      "expr": { "kind": "first",  "node": "assignee.match" } },
        { "as": "comment_count", "expr": { "kind": "scalar", "node": "comments.count" } } ] },

    "out": { "kind": "project", "input": "boards", "spread": ["b"],
      "fields": [
        { "as": "owner", "expr": { "kind": "first", "node": "owner.match" } },
        { "as": "tasks", "expr": { "kind": "array", "node": "tasks.shaped" } } ] }
  },
  "root": { "node": "out", "cardinality": "many" }
}
```

No `include`, no `parent`, no `children`, no aggregate-include — only scans,
filters, projections, an order, a slice, an aggregate, and four crossings.
If the IR ever needs a special case to express this query, the abstraction
has failed.

Nobody hand-writes this. The generated clients keep their fluent surface —
`db.Boards.Query().IncludeTasks(...)` — and emit the graph; a SQL compiler,
a graphical builder, or a terser DSL can emit it later. The canonical IR
optimises for unambiguous semantics, not keystrokes.

## Binding

The wire graph decodes into *unbound* IR: table and column names, raw JSON
literals, scope labels. The binder — the engine's front door — produces
*bound* IR in one recursive walk:

```text
cycle detection → scope resolution → name/ID binding → literal coercion
→ type inference → cardinality inference → free scopes → validation
```

- Names resolve to catalog IDs; scope labels to dense scope IDs. Crossings
  keep the outer scope stack visible — that is how correlation binds.
- **The binder owns all literal coercion.** A `lit` arrives as a raw JSON
  scalar and is typed by the column it meets (a JSON number becomes int64 or
  float64 by the column's type, never by guessing); a NULL literal adopts the
  column's type. There is no numeric widening in comparisons this arc.
- Type inference: comparisons are Bool (nullable iff an operand is nullable —
  that nullability *is* UNKNOWN); `count` int64 never NULL; `sum/min/max` the
  argument's type, nullable; `avg` float64, nullable; `First` → nullable row;
  `Scalar` → the single column's type, nullable; `Array` → non-nullable array.
- Validation is exhaustive and bind-time: unknown tables/columns, out-of-scope
  references, duplicate scope labels, non-Bool predicates, type mismatches,
  `Scalar` over a relation whose arity ≠ 1, aggregate term rules, columns
  above an Aggregate resolving only to groups/terms, duplicate projection
  fields, cycles. The planner and executor trust bound IR completely.

## Physical mapping

The logical graph names no indexes and no keys. Planning chooses physical
operators; the invariant carried over from v1, now made structural:

> The access path only narrows which keys are scanned. The full original
> predicate rides above the scan — `Filter(IndexRangeScan(...), pred)` — and
> is re-evaluated on every fetched row. Path choice can never change results.

**Access selection**, generalising v1's equality-only `chooseAccess`:

1. Equality constraints covering the whole primary key → point `Get`.
2. Otherwise the index with the longest leading equality prefix, now **plus
   one trailing range bound** — `lt/lte/gt/gte` predicates finally drive
   access paths, as KV scan bounds built from the order-preserving encoding.
3. Tie-break: when an `Order`+`Slice` sits above, an index whose column order
   satisfies the required ordering wins.
4. Otherwise, full table scan.

**Ordered-index pushdown** (deliberately limited): index scans provide their
column order (ascending only — the KV has no reverse scan; `desc` always
sorts). When provided ⊇ required ordering and a `Slice` is present, the sort
disappears and the scan stops after `offset+limit` accepted rows. No general
Top-N heap.

**Batched decorrelation** — the N+1 kill. A projection field whose crossing
is *key-correlated* (its sub-relation's only free references are
`inner.col = outer.col` equalities) executes batched:

```text
for the current batch of outer rows:
    collect the distinct outer key tuples        (NULL key ⇒ empty result now)
    fetch the sub-relation once per DISTINCT key (index scan / PK get)
    group results by key; attach to each outer row
    recurse: grandchildren batch across this inner batch
```

The to-parent pattern (correlation keys covering the inner PK) becomes
deduplicated point gets. A per-key `Slice` ("first 20 tasks *per board*")
keeps per-key scans with stop-early — a merged multi-key scan cannot honour a
per-key limit and is deferred. Generally-correlated sub-relations (non-key
predicates over outer scopes) fall back to per-row nested evaluation.
Batched and nested execution are result-equivalent by construction, and the
conformance suite asserts it.

**Execution** is a pull-operator tree (`Next(ctx) (Frame, bool, error)`), one
operator per physical node. Operators may materialise internally (sort,
aggregate, batched projection do); nothing in the contract requires it, so
streaming remains a later optimisation, not a rewrite. Results build a typed
value tree — `Null | Scalar | Object | Array` — replacing the four-map record;
the JSON renderer preserves today's output exactly (a NULL to-one field is
present-with-null; children are arrays, never null; folds are scalar objects).

## What this deliberately does not do (this arc)

- No streaming pipeline end-to-end; no vectorised execution.
- No cost model, statistics, or join reordering; access selection is the
  rule-based ranking above. Joins execute as nested loops (inner, left).
- No merged multi-key batched scans; no index-only scans; no descending scans.
- No `Call` registry, no expression arithmetic on the wire from the generated
  clients (the grammar carries it; nothing emits it yet).
- No `HAVING`, `DISTINCT`, window functions, recursive queries, CTEs. GROUP BY
  ships as `Aggregate.groups` with minimal codegen sugar.
- No numeric widening in comparisons; no configurable NULL-distinctness
  (NULLs-distinct is the one behaviour).

Each is an extension point, not a redesign: grouping already lives in
`Aggregate`; semi/anti joins are enum values; a `Correlate`-free IR
decorrelates into whatever join strategies arrive later; the wire's string-id
graph admits DAG sharing without a format change.

## Disposition of v1's POC deviations

| v1 deviation | v2 disposition |
|---|---|
| Two-valued NULL logic (`NOT(x=1)` matches NULL) | **Fixed** — K3, Filter keeps TRUE |
| NULLs participate in unique constraints | **Fixed** — NULLs distinct |
| Only equalities drive access paths | **Fixed** — trailing range bounds |
| Includes are N+1 by construction | **Fixed** — batched decorrelation |
| Full materialisation, no pushdown | **Partly** — stop-early slices over ordered index scans; operators still materialise internally |
| Registered-but-empty index after failed unique backfill | **Fixed** (standalone, before this design lands) — registration + backfill in one transaction |
| Aggregates reject order/limit/include structurally | **Dissolved** — legal algebra; binder rules replace the blanket rejection |
| Deletes restrict, never cascade | Unchanged (mutation path is out of scope) |
