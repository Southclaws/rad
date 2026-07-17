# Set operations as first-class relation operators

Status: idea, not scheduled. Companion to `distinct` and recursive bindings.

Recursive bindings introduced the first place where duplicate handling became a
semantic concern (`accumulation: all | new`), and the planned unary
`distinct` operator introduces the second. This ADR addresses the remaining
piece: binary set operations.

The goal is **not** to expose SQL's syntax. The goal is to expose the
underlying relational algebra directly.

## The problem

SQL's syntax obscures the actual algebra.

```sql
SELECT ...
UNION
SELECT ...
```

is not one special construct. It is one member of a family of binary relation
operators:

- union
- intersection
- difference

Likewise, SQL overloads `DISTINCT` across several unrelated concepts:

- unary relation deduplication (`SELECT DISTINCT`)
- set-operation semantics (`UNION DISTINCT`)
- aggregate input deduplication (`COUNT(DISTINCT ...)`)

Those are different logical operations that merely happen to involve duplicate
elimination.

LIR should model the operators themselves rather than inherit SQL's wording.

## Decision

Introduce binary **set operations** as ordinary relation operators.

They are siblings of `join`, not variants of it.

```
scan
rows

filter
project
join
aggregate
order
slice
distinct

union
intersect
except
```

The recursive binding remains separate. Recursive evaluation is not a binary
set operation.

## Operators

Three binary relation operators are introduced.

### Union

Produces the union of two relations.

```
Union(left, right)
```

Modes:

- `all` — preserve multiplicity.
- `distinct` — remove duplicate rows.

### Intersect

Produces only rows present in both relations.

```
Intersect(left, right)
```

Modes:

- `all`
- `distinct`

### Except

Produces rows present in the left relation but not the right.

```
Except(left, right)
```

Modes:

- `all`
- `distinct`

Whether `all` variants of `intersect` and `except` are implemented in v1 is an
implementation concern. The logical model should accommodate them from the
start.

## Distinct remains unary

The unary `distinct` operator is independent.

```
Distinct(input)
```

It converts an arbitrary bag relation into the corresponding set relation.

It is **not** implemented in terms of `Union`, nor is recursive accumulation
implemented in terms of `Distinct`.

Instead:

- `Distinct`
- `Union(..., mode=distinct)`
- `Intersect(..., mode=distinct)`
- `Except(..., mode=distinct)`
- recursive `accumulation: new`

all share one canonical definition of row identity.

## Row identity

All duplicate-sensitive operators use identical full-row identity semantics.

Two rows are identical when every corresponding datum has identical canonical
representation.

This identity differs from predicate equality:

- `NULL` equals `NULL`.
- Structural values compare structurally.
- Type is part of identity.

The planner and executor should expose one reusable canonical row identity
primitive rather than separate implementations of duplicate elimination.

## Ordering

None of the set operations preserve logical ordering.

Their outputs are ordinary unordered relations.

Any observable ordering requires an explicit `order` operator after the set
operation.

This allows either hash- or sort-based implementations without affecting
observable semantics.

## Type rules

All three operators require compatible inputs.

Both inputs must produce identical row types:

- same number of columns;
- same column order;
- compatible kinds;
- compatible nullability.

The output row type is identical to the reconciled input row type.

Unlike `join`, no new columns are introduced.

## Relationship to recursive bindings

Recursive bindings intentionally do **not** expose `union`.

Instead they expose:

```
accumulation: all | new
```

which answers a different semantic question:

> When each recursive iteration produces rows, which become part of the
> recursive result?

This avoids conflating recursive fixpoint semantics with binary relation
operations.

Internally, recursive accumulation may reuse the same canonical row identity
implementation as `Union(..., distinct)` and `Distinct`, but it remains a
separate language construct.

## Optimisation opportunities

Representing set operations explicitly gives the planner algebraic rewrite
rules.

Examples:

```
Distinct(Distinct(x))
⇒ Distinct(x)
```

```
Distinct(Union(all, a, b))
⇒ Union(distinct, a, b)
```

```
Union(distinct, Distinct(a), Distinct(b))
⇒ Union(distinct, a, b)
```

Similar identities exist for intersection and difference.

These belong naturally in a future optimiser and are difficult to express if
the language models only SQL syntax.

## Wire shape

The precise schema is deferred.

The expected shape is:

```yaml
UnionNode:
  left: node
  right: node
  mode: [all, distinct]

IntersectNode:
  left: node
  right: node
  mode: [all, distinct]

ExceptNode:
  left: node
  right: node
  mode: [all, distinct]
```

These are ordinary members of the `Node` union.

## Non-goals

This ADR does not introduce:

- SQL syntax;
- recursive evaluation;
- aggregate `distinct`;
- unary `distinct` (covered separately).

It establishes only the binary relation operators.

## Decision record

- Binary set operations are first-class relation operators.
- They are independent of recursive bindings.
- Unary `distinct` remains a separate operator.
- Recursive `accumulation: new` remains a separate concept.
- All duplicate-sensitive operations share one canonical row identity
  definition and implementation.
- LIR models the relational algebra directly rather than SQL's surface syntax.
