# Design: LIR — the relation tree

Status: normative, intentionally unstable POC contract

LIR is currently an internal API with no compatibility promise or external
consumers. The schema and this document describe the current contract; they
may change freely while the design is being hardened.

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
from physical access paths, the residual filter as the source of truth. The
current LIR keeps all of that and fixes the architecture underneath with one idea:

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

**`Expr`** — a computation that produces exactly one typed datum in some
scope. Most expressions are scalar; `First` and `Array` produce row and array
datums. Expressions may *consume* relations only through an explicit crossing
that states how many rows become one datum.

Every relation node obeys the same laws:

```go
type Relation interface {
    Output() RowType        // the row type this relation produces; every
                            // field carries a dense SlotID
    Inputs() []Relation     // child relations
    FreeSlots() SlotSet     // slots referenced but not produced beneath
    Produced() SlotSet      // every slot defined beneath, incl. intermediates
    Card() Cardinality      // {Min, Max} bounds, Max may be Unbounded
}
type Expr interface {
    Type() Type             // inferred type, including nullability
    FreeSlots() SlotSet
}
```

**Relational closure is a law, not an aspiration**: the output of every
relation node must be usable as an ordinary input relation, which means every
output attribute must be addressable by later operators. In the *unbound* IR
attributes are named — a `Column{scope, name}` names a column of a bound
scope, and row-producing nodes (`scan` always; `project` and `aggregate` when
their outputs are referenced downstream) bind the scope labels those names
resolve against. In the *bound* IR the binder assigns every relation output a
dense **slot** per field and rewrites every column reference to a
`SlotRef(slot)` — names and scopes exist only at the binding boundary. A
`Filter` above an `Aggregate` references the fold's `total` as a slot the
aggregate produced, exactly as it would reference a scanned column.

`FreeSlots` is the whole correlation story. There is no `Correlate` node: a
sub-relation that needs slots supplied by an enclosing evaluation scope is
*correlated*, as a derived property. The binder computes it; the planner
exploits it. Correlation is not materialisation. Materialisation happens only
when a crossing or the root converts a relation into a datum.

Terminology is exact throughout this contract: `Value` is a typed scalar;
`Datum` is null, scalar, row, or array; `Expr` computes one datum; a crossing
is the explicit relation-to-datum boundary.

### The discipline

Before adding any node to this IR, ask: **could this be expressed as an
ordinary relation?** Every time the answer is yes, resist the special node.
SQL is the cautionary tale — a different spelling for every variation of the
same underlying problem. All of these are already expressible here:

| SQL spelling | LIR expression |
|---|---|
| `EXISTS (subquery)` | `Exists(rel)` |
| `JOIN` | `Join` — already minimal |
| `JSON_AGG` / `ARRAY_AGG` | `Array(rel)` in a projection field |
| scalar subquery | `Scalar(rel)` |
| `SELECT ... LIMIT 1` lookup | `First(rel)` / `Slice` |
| `HAVING` | `Filter(Aggregate(...), predicate)` |

`IN`, `ANY`, and `ALL` are deliberately not claimed as `Exists` rewrites:
NULL makes those rewrites observably wrong outside a filtering context. Their
NULL-correct semantics are a future expression feature. Correlated crossings
produce nested values; LIR does not provide relational `LATERAL`/`APPLY`
flattening.

The IR converges on four primitives — relations, expressions, cardinality
crossings, projection — and everything else must be a consequence of those.
A proposed feature that cannot be phrased in them is either a genuinely new
primitive (rare; argue for it in a design doc) or a frontend nicety that
belongs in a compiler above the IR, not in the IR.

## Relation operators

```text
Scan      { table, scope }            introduce a table; bind a scope label
Filter    { input, predicate }        keep rows where predicate is TRUE (3VL)
Project   { input, fields }           establish a new row type; fields are exprs
Join      { left, right, kind, on }   inner | left  (semi/anti reserved)
Aggregate { input, groups, terms }    fold; empty groups ⇒ exactly one row
Order     { input, terms }            logical ordering; NULLs first asc, last desc
Slice     { input, offset, limit }    limit absent = unlimited; 0 = zero rows
```

Notes:

- `Scan` always binds a scope; `Project` and `Aggregate` — the nodes that
  establish new row types — may bind one too, and must when a later operator
  references their outputs by name (`Order(stats, Column(stats, total))`).
  Scope labels exist only in the unbound IR; the binder resolves every
  reference to an output slot.
- `Filter` keeps only rows whose predicate evaluates to `TRUE` — not
  `UNKNOWN`. This is the load-bearing 3VL rule.
- `Project` is not column selection; it establishes a new row type. Aliases,
  computed values, and nested materialisation all live here.
- `Aggregate` with empty `groups` is the global fold: cardinality exactly 1
  (the `count → 0, sum/avg/min/max → NULL` empty-set rules are unchanged from
  previous aggregation design). With groups, it is GROUP BY: one row per distinct group.
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
First(rel)   : Expr<Row?>            the row as a nested object, or NULL
Scalar(rel)  : Expr<T?>              single-column, single-row relation → its
                                     value, or NULL when there is no row
Array(rel)   : Expr<Array<Row>>      all rows as a nested array; empty, never NULL
```

`First` and `Scalar` must be deterministic — a KV scan's encounter order is
physical, never logical, and access-path choice must not change results. So
the binder enforces, statically:

- `First(rel)` is legal iff `rel.Card().Max ≤ 1` (a unique-key filter, a
  global aggregate, a `Slice` of 1) **or** `rel` carries an explicit logical
  `Order`. Take-first over an *ordered* relation is deliberate row selection;
  take-first over an unordered one is a plan choice leaking into results, and
  is rejected. (`Any(rel)`, an explicitly nondeterministic escape, is
  reserved but not built.)
- `Scalar(rel)` is a cardinality *assertion*: the relation must have exactly
  one output column and statically at most one row. "First scalar" is spelled
  out as the composition `Scalar(Slice₁(Order(...)))` rather than implied.

`Slice` itself is explicitly positional: slicing an unordered relation is a
declared-arbitrary selection (the SQL `LIMIT` without `ORDER BY` contract),
and the path-independence invariant carries that one documented exception.
A relation can therefore appear almost anywhere — inside a projection field,
inside a filter predicate — but never *as* a scalar without declaring the
conversion, and never with plan-dependent contents.

Shaping falls out: a projection field whose expr is `First(sub)` renders as a
nested object (or JSON null), `Array(sub)` as a nested array, `Scalar(sub)` as
a naked value, `Exists(sub)` as a boolean. The old include taxonomy is gone.

### The root

A query is a relation plus a root materialisation mode:

```text
Query { root: Relation, cardinality: many | first | exactly_one | scalar }
```

`many` → array of objects; `first` → object or null; `exactly_one` → object,
error otherwise; `scalar` → a single value or null. Scalar is the same
assertion as the expression crossing: the root must have exactly one output
column and be statically at most one row. Ordering a multi-row relation makes
`first` deterministic but does not make it a valid scalar.

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

The binder resolves `b.id` to a slot produced outside `boardTasks`, so
`FreeSlots(boardTasks) ≠ ∅` — the relation is correlated. It appears in a
projection over boards:

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

`POST /query` takes a strict LIR relation forest encoded as a flat map of
caller-chosen node ids. References among ordinary nodes are plain strings,
and every definition is inline: node ids have no sharing, memoisation, or
materialisation identity. The preflight validates the document as a forest —
one tree rooted at `root.node`, one tree per binding root — rejecting
cycles, dangling references, shared definitions, and unreachable
definitions.

Nodes and expressions are closed JSON Schema `oneOf` unions selected by
`kind`. Unknown fields, fields belonging to another variant, invalid operators,
and missing required payloads are rejected before binding.

LIR is an independent contract surface: OpenAPI describes `/query` as a raw
JSON body and does not import these definitions. Schemancer generates the Go
tagged unions from `rad/protocol/lir.schema.json`; the server validates the raw
document against that same schema before decoding or planning it.

```text
Query   { nodes: { <id>: RelNode }, bindings?: { <name>: {node} },
          root: { node, cardinality } }
RelNode kind ∈ scan filter project join aggregate order slice ref
Expr    kind ∈ lit col unary binary call cast exists first scalar array
```

- `scan` requires `scope`, unique per query. `project` and `aggregate` accept
  an optional `scope` binding their *output* row, required whenever a later
  node references their fields by name — relational closure on the wire.
- `project` has `spread: [scopes]` ("all columns of these scopes first") plus
  `fields: [{as, expr}]`. Name collisions — explicit field vs explicit field,
  explicit field vs spread-produced column, spread vs spread across scopes —
  are all rejected at bind time.
- `slice.limit` omitted means unlimited; an explicit `0` means zero rows. (The
  old wire's "0 = unlimited" convention ends at the compat boundary — an IR
  must be able to say *no rows*.)
- `and`/`or` are binary; clients left-fold chains.
- Crossings (`exists|first|scalar|array`) carry `node: <id>` referencing the
  sub-relation.
- Every non-root node has exactly one consumer; every declared node must be
  reachable from the root.

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

## Relation bindings

Bindings are the one deliberate sharing construct — derived relations as
first-class relational values. The normative kernel:

> A binding denotes one statement-local relational value. Every reference
> observes that same bag of rows and values. Each reference exposes it
> under a fresh local alias. The physical planner may evaluate that value
> by inlining, duplicating, streaming, caching, or materialising the
> binding — anywhere doing so is observationally indistinguishable.

The model mirrors base tables exactly: one definition (the binding's tree),
one value per statement (a *committed* bag — a binding over a
declared-arbitrary body such as an orderless `slice` resolves the choice
once), N occurrences (`ref` nodes, each a fresh scope over the same value,
as two scans of one table are two variables over one snapshot). A self-join
of a binding is therefore genuinely a self-join: the diagonal exists no
matter how arbitrary the body.

Rules, each pinned by tests:

- `bindings` maps names to root nodes within `nodes`; ordinary edges remain
  tree-shaped, `ref` edges point into the binding namespace. Binding trees
  may themselves reference other bindings: those references form a separate
  acyclic dependency graph *over* the relation forest — the forest is the
  set of ordinary node trees, the dependency graph is the overlay. The
  preflight rejects unused bindings, refs to unknown bindings, and binding
  cycles (recursion is a future construct, never an accident).
- Bindings are closed: a body referencing scopes outside its own tree fails
  to bind. Interior scopes never leak through a reference — the public
  contract is the output schema (names, types, nullability), which must be
  well-formed (duplicate output columns are rejected with the
  project-explicitly remedy).
- References are uniformly 0..many regardless of the body: static
  cardinality is not part of the contract, so `first`/`scalar` consumers of
  a reference bring their own order or slice.
- Commit-once is the semantic obligation, not evaluate-once. The planner
  picks the strategy, visible in EXPLAIN: a binding with exactly one
  occurrence streams inline at that occurrence (replay — its single
  evaluation is the commitment), while multi-reference bindings materialise
  once in dependency order before the root runs, which also keeps nested
  multi-reference bindings linear at execution. The binder derives
  *plan-choice sensitivity* (can two valid plans commit different bags?)
  conservatively; it is what forbids per-occurrence plan divergence if
  multi-reference replay of insensitive bodies ever lands.

## Name binding

The wire tree first passes strict tagged-union and exact-forest validation,
then decodes into *unbound* IR: table and column names, raw JSON literals,
scope labels. The binder — the engine's front door — produces
*bound* IR in one recursive walk:

```text
scope resolution → name/ID binding → slot assignment → literal coercion
→ type inference → cardinality inference → free slots → validation
```

(Cycle rejection lives in the wire's graph decoder, the only place string
node references exist — unbound IR nodes are value structs and cannot form
cycles at all.)

- Names resolve to catalog IDs; every relation output gets dense slot IDs and
  every column reference becomes a `SlotRef`. Crossings keep the outer scope
  stack visible — that is how correlation binds — and a reference into a
  `project`/`aggregate` output requires that node to have bound a scope.
- Cardinality inference knows uniqueness: a `Filter` whose equality conjuncts
  cover a unique key (primary key or unique index) of its scan has `Max = 1`
  — this is what lets the to-parent pattern (`First` over an FK→PK filter)
  pass the determinism rule statically.
- `Order` gains a deterministic tie-breaker at bind time when the output
  carries a known unique key (a scan's primary key, an aggregate's group
  slots): the key is appended as final ascending terms, so tied rows order
  identically under every access path. Without a known unique key, ties are
  documented as unspecified.
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
  `Scalar` over a relation whose arity ≠ 1 or whose static `Max > 1`,
  `First` over an unordered multi-row relation, aggregate term rules, columns
  above an Aggregate resolving only to groups/terms, projection name
  collisions (explicit×explicit, explicit×spread, spread×spread), dependent
  join inputs (a join side referencing its sibling), crossings inside a join
  condition. The planner and executor trust bound IR completely.

## Physical mapping

The logical graph names no indexes and no keys. Planning chooses physical
operators; the access-path invariant is structural:

> The access path only narrows which keys are scanned. The full original
> predicate rides above the scan — `Filter(IndexRangeScan(...), pred)` — and
> is re-evaluated on every fetched row. Path choice can never change results.

**Access selection**, generalising the initial equality-only `chooseAccess`:

1. Equality constraints covering the whole primary key → point `Get`.
2. Otherwise the index with the longest leading equality prefix, now **plus
   one trailing range bound** — `lt/lte/gt/gte` predicates finally drive
   access paths, as KV scan bounds built from the order-preserving encoding.
3. Tie-break: when an `Order` sits above, an index whose column order
   satisfies the required ordering wins.
4. Otherwise, full table scan.

**Ordered-index pushdown** (deliberately limited): an index scan's *provided*
ordering is the index columns **after the equality-fixed prefix**, then the
primary-key suffix, all ascending — an index on `(board_id, priority)` with
`board_id = X` provides `priority` order; without that equality it provides
`(board_id, priority)` order, not `priority`. (The KV has no reverse scan;
`desc` always sorts.) When provided ⊇ required, the sort disappears — and a
`Slice` above then stops the scan after `offset+limit` accepted rows. No
general Top-N heap.

**Deduplicated correlated execution** — how correlation runs, stated
honestly. The planner extracts every crossing — from a projection field, a
filter predicate, an order term, an aggregate argument — into an
**`AttachSpec`** writing a fresh slot, grouped under an **`AttachExec`**
operator below the consumer; the containing expression evaluates over the
slot, so wrapping a crossing in `NOT` or arithmetic cannot change how it
executes. A *key-correlated* attach (its sub-relation's only free references
are `inner.col = outer.col` equalities) executes grouped over the batch:

```text
for the current batch of rows:
    collect the distinct outer key tuples        (NULL key ⇒ empty result now)
    instantiate the attach plan once per         DISTINCT key today
    group results by key; attach to each row
    recurse: grandchildren batch across this inner batch
```

This *dissolves* N+1 semantically — no per-outer-row plan recursion, and
duplicate keys are fetched once — but for N distinct parents the storage work
is still N lookups/scans. The physical seam is explicit so that changes
without touching the IR: `AttachExec` is where true multi-key batching —
merged range scans, concurrent prefix scans, one broad scan partitioned by
key — lands later.

The to-parent pattern (correlation keys covering the inner PK) becomes
deduplicated point gets — already ahead of today's per-row fetches. A
per-key `Slice` ("first 20 tasks *per board*") keeps per-key scans with
stop-early — a merged multi-key scan cannot honour a per-key limit.
Generally-correlated sub-relations (non-key predicates over outer scopes)
fall back to per-row nested evaluation. Grouped and nested execution are
result-equivalent by construction, and the conformance suite asserts it.

Crossings are never scalar-evaluator callbacks: every `Array`, `First`,
`Scalar`, and `Exists` compiles to an explicit sub-plan inside an
`AttachExec` (deduplicated when key-correlated, per row when general) — a
thousand-row `Array` is a physical plan the executor can see, not a
recursive function call hiding one. Expression evaluation itself is pure:
no context, no I/O, no relation evaluator behind an operand.

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
- No true multi-key storage batching (merged or concurrent range scans) —
  `AttachExec` is the seam, and it loops per distinct key. No index-only
  scans; no descending scans.
- No `Call` registry, no expression arithmetic on the wire from the generated
  clients (the grammar carries it; nothing emits it yet).
- No dedicated `DISTINCT`, window functions, recursive queries, quantified
  comparisons, or relational `LATERAL`/`APPLY`. `HAVING` is already ordinary
  `Filter(Aggregate(...))`; GROUP BY is `Aggregate.groups`; CTE-style
  sharing is relation bindings (above).
- No correlated bindings — structurally: document-level bindings have no
  enclosing scopes at their definition site, so closedness is a theorem of
  the surface, not a restriction. References inside correlated contexts (a
  committed subset consulted per outer row) work. Per-environment
  commitment semantics are reserved for a future explicitly-parameterised
  construct. No multi-reference replay (multi-reference bindings always
  materialise; the sensitivity classification gates that future
  refinement).
- No numeric widening in comparisons; no configurable NULL-distinctness
  (NULLs-distinct is the one behaviour).

Each is a future feature, not a reason to weaken the contract: relation
features remain relation operators, expression features compute datums, and
recursion must introduce explicit semantics rather than turning arbitrary
wire cycles into meaning.

## Disposition of earlier POC deviations

| Earlier deviation | Current disposition |
|---|---|
| Two-valued NULL logic (`NOT(x=1)` matches NULL) | **Fixed** — K3, Filter keeps TRUE |
| NULLs participate in unique constraints | **Fixed** — NULLs distinct |
| Only equalities drive access paths | **Fixed** — trailing range bounds |
| Includes are N+1 by construction | **Dissolved semantically** — no per-row plan recursion; keys deduplicated and grouped. Physically still one lookup per distinct key, behind the `AttachExec` seam where true batching lands later |
| Full materialisation, no pushdown | **Partly** — stop-early slices over ordered index scans; operators still materialise internally |
| Registered-but-empty index after failed unique backfill | **Fixed** (standalone, before this design lands) — registration + backfill in one transaction |
| Aggregates reject order/limit/include structurally | **Dissolved** — legal algebra; binder rules replace the blanket rejection |
| Deletes restrict, never cascade | Unchanged (mutation path is out of scope) |
