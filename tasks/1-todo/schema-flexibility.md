## Overall verdict

Yes: this is a strong LIR shape. I would be comfortable building a binder, logical planner, SQL frontend, and direct API frontend against this.

The big architectural win is the separation between **relation operators** and **scalar expressions**, with explicit **relation-to-scalar crossings**. That is exactly the kind of boundary you want if RAD is trying to avoid becoming “SQL encoded as JSON.” The schema feels like a real query algebra, not a syntax tree for SQL.

I would not call it fully frozen yet, though. The JSON Schema is clean, but several of the most important rules are currently only implicit:

- graph acyclicity and single-consumer validity;
- scope visibility and correlation rules;
- output shape/naming rules;
- aggregate legality;
- scalar subquery cardinality;
- bag/set/null/order semantics;
- whether ordering is a logical property or only a materialisation property.

That is normal for an IR, but those rules need to become part of the LIR spec before recursion lands.

---

# 1. Is this a good IR abstraction over relational algebra?

Yes, with one caveat: it is better described as a **logical query algebra** than pure classical relational algebra.

The core mapping is good:

| LIR node    | Algebraic role                                                       |
| ----------- | -------------------------------------------------------------------- |
| `scan`      | base relation                                                        |
| `filter`    | selection                                                            |
| `project`   | projection / rename / extend                                         |
| `join`      | theta join plus left outer join                                      |
| `aggregate` | grouping / aggregation                                               |
| `order`     | ordering operator, outside pure RA but necessary for query semantics |
| `slice`     | offset/limit, also outside pure RA but necessary                     |
| crossings   | scalar subqueries / decorrelation boundary                           |

The most important design choice is this:

> `Expr` computes one datum. `Node` computes one relation. Crossings are the only door from relation back into scalar expression space.

That is a very good abstraction boundary.

It gives you a clean place to implement correlated subqueries, `exists`, scalar subqueries, array materialisation, and later decorrelation rewrites. It also prevents scalar expressions from quietly becoming mini query plans unless they pass through an explicit crossing.

The second excellent choice is **scope-qualified columns**:

```yaml
{ kind: col, scope: u, column: id }
```

That is much better than positional column references or SQL-style ambiguous names. It gives generated code a stable way to address attributes and makes joins/project composition much cleaner.

The flat `nodes` map is also reasonable. Even though the logical shape is a tree, named node definitions make the wire format easier to generate, inspect, validate, and debug. The comment that IDs “carry no sharing identity” is important. Without that rule, correlated subquery scope becomes much harder, because the same node could be referenced from two different environments.

The caveat: the schema says “single-consumer relation tree,” but JSON Schema does not enforce that. The binder/validator must enforce it.

---

# The biggest thing to tighten: scope semantics

The schema has the right pieces, but the spec needs to define a visibility model very explicitly.

I would define something like this:

1. Every relation node has an output shape: a set of visible scopes, each scope containing named columns.
2. `scan` introduces one scope.
3. `join` exposes the union of left and right scopes.
4. `filter` exposes its input scopes unchanged; its predicate may reference only input scopes plus valid outer scopes.
5. `project` exposes explicitly spread scopes plus optionally one newly-created computed scope.
6. `aggregate` exposes only its aggregate output scope; input scopes are not visible after aggregation unless deliberately reintroduced through group terms.
7. A crossing node may reference the current expression environment as outer scope.
8. A normal join’s right input may not reference the left input. Only a future `apply` / `lateral` node should allow that.

That last point is important.

Right now, the raw schema does not stop this:

```yaml
right_subquery:
  kind: filter
  input: some_scan
  predicate:
    kind: binary
    op: eq
    left: { kind: col, scope: right_scope, column: x }
    right: { kind: col, scope: left_scope, column: y }
```

Whether that is legal depends entirely on semantic validation. I would keep normal `join` non-lateral and introduce a separate `apply` node later. Do not let arbitrary relation children reference sibling scopes, or all joins become secretly lateral and the optimiser has a harder job.

---

# Important schema-level issues I would fix now

## 1. `project.fields` should require `scope`

Currently this is valid:

```yaml
kind: project
input: x
fields:
  - as: name
    expr: ...
```

But where does `name` live? There is no output scope for computed fields unless `scope` is present.

I would add:

```yaml
dependentRequired:
  fields: [scope]
  scope: [fields]
```

For `ProjectNode`, I would make `scope` mean “the namespace for newly computed fields.” If there are no computed fields, `scope` should be absent.

So:

```yaml
project spread only         => no new scope
project fields only         => new scope required
project spread plus fields  => spread old scopes plus new computed scope
```

That is clean.

---

## 2. `aggregate.scope` should probably be required

Aggregation creates a new relation shape. I would not let aggregate outputs float around unscoped.

Current `AggregateNode`:

```yaml
required: [kind, input]
```

I would change it to:

```yaml
required: [kind, input, scope]
```

Then every group term and aggregate term becomes a named column inside that output scope.

---

## 3. `GroupTerm.as` should probably be required

For an LIR, implicit output naming is usually a mistake.

This:

```yaml
groups:
  - expr: { kind: col, scope: u, column: id }
```

requires the binder or downstream consumer to infer a name. That is SQL-ish behaviour. I would make group terms explicit:

```yaml
GroupTerm:
  required: [as, expr]
```

Then `aggregate` can also serve as your current minimal `distinct` mechanism:

```yaml
kind: aggregate
input: users
scope: distinct_users
groups:
  - as: country
    expr: { kind: col, scope: u, column: country }
```

---

## 4. `AggTerm.arg` needs conditional validation

Right now this is schema-valid:

```yaml
{ fn: sum, as: total }
```

But `sum`, `avg`, `min`, and `max` need an argument.

I would define:

- `count` with no `arg` means `count(*)`;
- `count` with `arg` means count non-null values;
- all other aggregate functions require `arg`.

This can be enforced semantically or with JSON Schema `if` / `then`.

---

## 5. `slice` currently allows a no-op

This is currently valid:

```yaml
{ kind: slice, input: x }
```

But it does nothing.

For a canonical low-level IR, I would require at least one of `offset` or `limit`:

```yaml
anyOf:
  - required: [offset]
  - required: [limit]
```

`limit: 0` should remain valid; it means return no rows.

---

## 6. `first` and `scalar` crossings need sharper semantics

You currently have:

```yaml
exists
first
scalar
array
```

This is expressive, but `first` and `scalar` need precise behaviour.

For example:

- Does `scalar` error on zero rows, or return `null`?
- Does `scalar` error on more than one row, or must the binder prove at most one row?
- Does `first` require one output column?
- Is `first` nondeterministic unless the subquery has an `order`?
- Is `array` an array of scalar values, rows, objects, or implementation-defined records?

I would strongly recommend that every crossing mode define:

| Mode     | Required relation shape                        | Empty result    | More than one row    |
| -------- | ---------------------------------------------- | --------------- | -------------------- |
| `exists` | any shape                                      | `false`         | `true`               |
| `scalar` | exactly one column, statically at most one row | probably `null` | binder/runtime error |
| `first`  | exactly one column?                            | probably `null` | take ordered first   |
| `array`  | one column or row object?                      | empty array     | collect all          |

Personally, I would consider removing `first` as a separate crossing.

You can express “first scalar value” as:

```text
scalar(order(...).slice(limit: 1))
```

or:

```text
scalar(slice(input, limit: 1))
```

assuming `scalar` means “zero or one row, one column.”

I would keep `exists` because it is semantically and physically important. I would keep `array` if nested API results are a first-class RAD feature.

---

# 2. Is it extensible enough for recursive queries, windows, lateral joins, etc.?

Mostly yes, but each of those features wants to be a **new relation operator**, not a hack inside `Expr`.

That is a good sign. It means your current abstraction has room to grow without contorting itself.

---

## Recursive queries

Do not implement recursion by allowing arbitrary cycles in the existing `nodes` graph.

That would weaken the cleanest part of the design: finite, single-consumer relation trees.

Instead, add recursion as an explicit controlled operator.

You will probably need two concepts:

```yaml
RecursiveNode:
  kind: recursive
  binding: string
  scope: string
  anchor: string
  step: string
  mode: union_all | union_distinct
```

And something like:

```yaml
RecursiveRefNode:
  kind: recursive_ref
  binding: string
  scope: string
```

The recursive node would mean:

1. evaluate `anchor`;
2. expose the previous iteration’s rows through `recursive_ref`;
3. evaluate `step`;
4. union the new rows into the result;
5. repeat until fixpoint or termination condition.

You will also need to decide whether `recursive_ref` refers to:

- the previous iteration’s delta/frontier;
- the accumulated result so far;
- or both via separate refs.

For tree traversal, frontier semantics are often the cleanest. For SQL-compatible recursive CTEs, you may want semantics closer to the recursive working table.

You will also need `union` or an implicit union inside `recursive`.

I would probably add a general union node anyway:

```yaml
UnionNode:
  kind: union
  mode: all | distinct
  inputs:
    - node_a
    - node_b
```

For recursion, `recursive` can either own the union semantics directly or lower into a `union` internally. But from an IR design perspective, `union` is fundamental enough that I would expect it to exist eventually.

Important: recursion should be the one place where the normal acyclic graph rule is relaxed, and only through `recursive_ref`.

---

## Lateral joins

Current crossings can express correlated scalar subqueries:

- `exists`;
- scalar lookup;
- nested array result;
- first value.

But they cannot express a relation-returning correlated join that contributes columns to the outer row.

For that, add a separate `apply` node:

```yaml
ApplyNode:
  kind: apply
  left: string
  right: string
  join: inner | left
```

Semantics:

- `right` may reference scopes from `left`;
- output is left scopes plus right scopes;
- `inner` drops left rows with no right rows;
- `left` null-extends right scopes when empty.

I would not extend normal `JoinNode` with `lateral: true`. A separate node is clearer and gives the optimiser a distinct decorrelation target.

So:

```text
join  = independent left/right inputs plus on predicate
apply = dependent right input evaluated per left row
```

That is a clean abstraction boundary.

---

## Window functions

I would not add window functions as ordinary `CallExpr`.

A window function is not row-local. It reads a partition of the relation. That violates the current nice invariant that `Expr` computes a datum from the current row, literals, functions, and explicit crossings.

Instead, add a relation operator:

```yaml
WindowNode:
  kind: window
  input: string
  scope: string
  fields:
    - as: string
      fn: string
      args: [Expr]
      partition_by: [Expr]
      order_by: [OrderTerm]
      frame: ...
```

A `window` node extends each input row with computed fields under a new scope, much like `project`, but with access to peer rows.

That preserves your current scalar-expression discipline.

You can later support:

- `row_number`;
- `rank`;
- `dense_rank`;
- `lag`;
- `lead`;
- moving aggregates;
- frame clauses;
- named windows.

But the core abstraction remains: windowing is a relation-to-relation operator, not a scalar call.

---

## Set operations

You do not currently have:

- `union`;
- `intersect`;
- `except`.

For many business queries, you can survive without them initially. But recursion almost certainly wants `union`, and a SQL frontend will eventually need all three.

I would add at least `union` before recursive queries.

A general set node could look like:

```yaml
SetNode:
  kind: set
  op: union | intersect | except
  mode: all | distinct
  left: string
  right: string
```

Or keep only this for now:

```yaml
UnionNode:
  kind: union
  mode: all | distinct
  inputs: [...]
```

For recursion, `union all` and `union distinct` matter.

---

## Parameters

This is not strictly relational algebra, but for RAD’s API-first goal, I think the LIR should probably have parameters.

Right now literals are embedded directly:

```yaml
{ kind: lit, value: 123 }
```

That is safe from SQL injection because there is no SQL string, but it still couples query shape to runtime values. For prepared plans, caching, query identity, and clean client APIs, you probably want:

```yaml
ParamExpr:
  kind: param
  name: string
  type: TypeRef
```

Then a query can be compiled once and executed with different bindings.

This feels very aligned with RAD’s design direction.

---

## Type extensibility

This part may become limiting:

```yaml
to: { type: string, enum: [text, int64, float64, bool] }
```

That is fine for a prototype, but the moment you add arrays, dates, timestamps, UUIDs, decimals, JSON, records, or nullable annotations, this enum becomes too small.

I would consider replacing it eventually with a `TypeRef`:

```yaml
TypeRef:
  oneOf:
    - { kind: primitive, name: text | int64 | float64 | bool | ... }
    - { kind: array, element: TypeRef }
    - { kind: record, fields: [...] }
    - { kind: named, name: string }
```

You do not need the whole type system now, but `cast.to` being a plain enum is one of the places where future pressure will show up quickly.

---

# 3. Can it be smaller?

Conceptually, it is already pretty small.

The relation-node set is close to the minimum useful query algebra:

```text
scan
filter
project
join
aggregate
order
slice
```

I would not remove any of those.

You could reduce schema size mechanically, but I would be careful not to reduce conceptual clarity.

---

## Safe simplification: collapse crossing variants

Instead of four separate schema definitions:

```yaml
CrossingExprExists
CrossingExprFirst
CrossingExprScalar
CrossingExprArray
```

you could use one:

```yaml
CrossingExpr:
  type: object
  additionalProperties: false
  required: [kind, mode, node]
  properties:
    kind: { type: string, const: crossing }
    mode: { type: string, enum: [exists, first, scalar, array] }
    node: { type: string, minLength: 1 }
```

That is smaller and probably cleaner.

The tradeoff is that separate `kind` values are nice for generated code pattern matching.

I lean slightly toward the collapsed version because all four variants have the same shape and differ only by materialisation mode.

---

## Possible simplification: remove `first` crossing

As mentioned above, if `scalar` over a `slice(limit: 1)` gives the same semantics, then `first` is redundant.

You could keep these crossings:

```text
exists
scalar
array
```

Then “first” is just a relation operation followed by scalar materialisation:

```text
order -> slice(limit: 1) -> scalar
```

That is algebraically cleaner.

But if `first` has distinct runtime semantics, such as “take the first row and return a row-shaped datum,” then keep it. Right now the schema description says every expression computes one typed datum, so I suspect `first` is probably removable.

---

## Do not collapse `unary`, `binary`, and `call` yet

You could make everything a call:

```yaml
{ kind: call, fn: "add", args: [...] }
{ kind: call, fn: "eq", args: [...] }
{ kind: call, fn: "not", args: [...] }
```

That would make the schema smaller.

I would not do it.

Keeping primitive operators closed gives you:

- simpler type checking;
- easier constant folding;
- easier predicate analysis;
- easier join condition extraction;
- easier index planning;
- less dependence on a dynamic function registry.

`call` is a good extension point. Primitive boolean/comparison/arithmetic ops deserve first-class representation.

---

## Do not merge `order` and `slice`

It is tempting to combine them:

```yaml
sort_limit: input
  terms
  offset
  limit
```

But keeping them separate is better.

These are different logical operations:

```text
order(input)
slice(input)
slice(order(input))
order(slice(input))
```

They have different semantics and different optimisation opportunities. A planner can later recognise `order + limit` as top-k.

---

## Do not remove `project`

Some IRs try to make projection implicit in every node. I would avoid that. `project` is doing important work here:

- pruning;
- computed fields;
- scope re-exposure;
- renaming;
- result shaping.

It is worth keeping as an explicit operator.

---

# The most important missing semantic decisions

Before freezing, I would write these into the LIR spec.

## Bag vs set semantics

Does RAD LIR default to bag semantics or set semantics?

Most practical database engines use bag semantics by default. Classical relational algebra is set-based. Your current operators do not specify which one applies.

This affects:

- `project`;
- `join`;
- `aggregate`;
- `union`;
- recursion termination;
- `array` crossings;
- `count`.

I would explicitly say:

> LIR relations are bags unless a distinct/grouping/set operator removes duplicates.

Then add `distinct` or use group-only `aggregate` as the initial dedupe mechanism.

---

## Null semantics

You have `null`, `is_null`, `is_not_null`, `eq`, `ne`, `and`, and `or`.

You need to define whether this is SQL-style three-valued logic or ordinary two-valued logic with nullable values.

For example:

```text
null = null
```

Does that evaluate to:

- `null` / unknown?
- `true`?
- `false`?

And in `filter`, does unknown pass or fail?

I would probably use SQL-compatible three-valued logic for predicates, because users will expect it once outer joins and nullable columns exist. But whatever you choose, it must be explicit.

---

## Ordering semantics

`order` and `slice` imply ordering matters.

You need to define:

- Does `filter` preserve input order?
- Does `project` preserve input order?
- Does `join` preserve any order?
- Does `aggregate` destroy order?
- Is `first` invalid or nondeterministic without `order`?
- Does `array` preserve order if its node is ordered?

I would define ordering as a logical stream property, not a property of pure relations. Then specify which nodes preserve or discard it.

A simple rule:

- `scan` has unspecified order;
- `filter`, `project`, and `slice` preserve input order;
- `order` establishes order;
- `join`, `aggregate`, `union`, and recursion produce unspecified order unless followed by `order`.

That is simple and safe.

---

## Cardinality rules

The root says:

```yaml
cardinality: many | first | exactly_one | scalar
```

This is a good API concept, but the terms need precise definitions.

For example:

| Mode          | Shape      | Empty         | Multiple rows                |
| ------------- | ---------- | ------------- | ---------------------------- |
| `many`        | relation   | empty result  | allowed                      |
| `first`       | row        | null? absent? | take first                   |
| `exactly_one` | row        | error         | error                        |
| `scalar`      | one column | null? error?  | error or statically rejected |

Also, the description says:

> Scalar requires one output column and statically at most one row.

That is a strong requirement. Good, but conservative binders may reject many queries unless they can prove uniqueness from:

- `limit: 1`;
- global aggregate without groups;
- key constraints;
- unique indexes;
- literal one-row relations;
- prior `exactly_one` guarantees.

That is fine, but document it.

---

# One thing I would seriously reconsider: raw JSON numbers

This part is understandable:

```yaml
Value:
  type: [string, number, boolean, "null"]
  format: raw
```

The comment says raw JSON is used so literal numbers keep full `int64` precision.

That may work in your Go/Schemancer path, but as a long-term wire format, JSON numbers are dangerous across languages. JavaScript clients, proxies, logging tools, and generic JSON parsers may round large integers.

For a durable database wire protocol, I would eventually prefer typed literals:

```yaml
{ kind: lit, type: int64, value: "9223372036854775807" }
{ kind: lit, type: float64, value: 1.5 }
{ kind: lit, type: text, value: "hello" }
{ kind: lit, type: bool, value: true }
{ kind: lit, type: null }
```

That is larger, but more correct.

A compromise is to keep the current compact `Value` for now and add `ParamExpr` soon, so most user values flow through typed parameters rather than inline JSON number literals.

---

# Recommended near-term changes

If I were making one final pass before recursive queries, I would do this:

1. **Keep the seven relation operators.**
2. **Add a semantic validity spec** for graph shape, scope visibility, output shape, type checking, ordering, nulls, and cardinality.
3. **Require `project.scope` when `fields` exists.**
4. **Require `aggregate.scope`.**
5. **Require `GroupTerm.as`.**
6. **Conditionally require `AggTerm.arg` for non-`count` aggregates.**
7. **Require `slice` to contain at least `offset` or `limit`.**
8. **Consider collapsing crossings into one `crossing` node with a `mode`.**
9. **Consider removing `first` crossing if `slice(limit: 1) + scalar` covers it.**
10. **Add `param` soon, even if not immediately.**
11. **Plan for `union` before recursive queries.**

---

# My direct answers

## 1. Is this a good IR abstraction over relational algebra?

Yes. It is a good logical algebra IR. It is not just SQL serialized into YAML/JSON. The relation/scalar split, scoped column references, explicit crossings, and small closed relation operator set are all strong choices.

The main thing missing is not another operator; it is the formal semantic layer around the schema.

## 2. Is this extensible enough?

Yes, provided future features are added as explicit operators:

- recursion: `recursive` plus `recursive_ref`, probably with `union`;
- lateral joins: `apply`;
- window functions: `window`;
- set operations: `union` / `intersect` / `except`;
- parameters: `param`;
- richer types: `TypeRef`.

The current design can absorb those without redesigning the core.

## 3. Can it be smaller?

A little, but I would not aggressively shrink it.

The safest reductions are:

- collapse the four crossing expression schemas into one `crossing` with `mode`;
- possibly remove `first` crossing;
- maybe later unify root materialisation and crossing materialisation terminology.

I would not collapse relation operators, and I would not turn all primitive expressions into generic function calls.

My strongest recommendation: freeze the **conceptual shape**, but not the exact schema yet. Tighten output naming, scope rules, aggregate rules, crossing cardinality, and ordering/null semantics first. Then this is a very solid LIR foundation.
