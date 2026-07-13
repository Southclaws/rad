would it make sense to actually just force order-by in relations for UX? because physical order is undefined anyway, and in sql doing most queries without ordering is a footgun

I think **forcing an explicit order at materialisation boundaries** could be a very good RAD UX decision.

I would **not** force every intermediate relation to be ordered.

Those are quite different policies.

## Why not make every relation ordered?

A relation is naturally unordered. Requiring every relation node to carry an order would:

- add noise to almost every query;
- imply operators preserve order when many should not;
- constrain join and aggregate planning;
- make harmless queries like `exists(...)` invent meaningless ordering;
- encourage people to treat order as inherent row identity.

For example, this does not need an order:

```text
exists(
  orders where orders.user_id = u.id
)
```

Nor does:

```text
aggregate orders:
  count(*)
```

An order contributes nothing to their result.

It would also complicate transformations. The optimiser would continually need to reason about whether order is required, preserved, destroyed, or merely accidental.

## But unordered output is absolutely a UX footgun

This is the real issue:

```text
query users
```

Users see rows arrive in some apparent stable order, build assumptions around it, and then an index, compaction, distributed execution change, or planner optimisation changes that order.

SQL technically warns you about this, but most APIs make it extremely easy to ignore.

RAD can do better.

I would distinguish:

```text
relations are unordered by default
```

from:

```text
observable collections must declare their ordering expectations
```

That means requiring explicit ordering where unorderedness becomes user-visible or semantically relevant.

## I would require order for these

### `first`

Absolutely.

An unordered `first` is conceptually broken:

```text
first(users)
```

First according to what?

Without an explicit order, it really means:

> Return whichever qualifying row the chosen physical plan happens to encounter first.

That should probably be invalid unless static cardinality proves at most one row.

So:

```text
first requires:
- an ordered input, or
- a proof that input cardinality <= 1
```

### Positive `offset`

Definitely.

```text
slice offset: 100
```

without ordering has no stable meaning.

It should be rejected.

### `limit`

Probably, with a nuance.

```text
limit: 10
```

without order can be useful when the caller genuinely means “any ten,” such as diagnostics, sampling-ish inspection, or existence-oriented work.

But for an application-facing query, it is normally a bug.

Possible policy:

```text
limit requires order unless explicitly marked arbitrary
```

For example:

```yaml
kind: slice
input: users
limit: 10
selection: arbitrary
```

Though I would avoid adding that until someone genuinely needs it.

### `array` crossings

Almost certainly.

If an array becomes application-visible JSON:

```json
{
  "comments": [...]
}
```

the array has observable order. Without an explicit order, response diffs, UI rendering, caches, and tests can fluctuate.

I would require an ordered relation for `array`, unless the type is explicitly an unordered collection—which JSON arrays are not.

### Paginated root results

Absolutely.

Offset pagination without ordering is nonsense, and cursor pagination needs an even stronger stable total order.

---

# Should `root.cardinality: many` always require order?

This is the interesting one.

I think there is a strong argument for **yes**, at least for the public wire interface.

It would make this invalid:

```yaml
root:
  node: users
  cardinality: many
```

unless the root relation has an explicit `order` operator.

That would force clients to state:

```text
I want users ordered by created_at, id
```

rather than accidentally consuming physical order.

This is opinionated, but it fits RAD extremely well:

- structured interface;
- correctness over historical convenience;
- reject ambiguous intent;
- make dangerous assumptions explicit.

It would also give generated APIs and frontends deterministic results by default.

However, I would not say every order must be a **total** order for ordinary collection output. Ordering by `created_at` alone is deterministic only if values are unique. If ten rows share the same timestamp, their relative order remains unspecified.

So you need to decide whether you require:

1. merely an order expression; or
2. a statically proven total order.

Requiring a proven total order everywhere may become frustrating. The binder would need key and uniqueness knowledge.

A practical policy might be:

> Collection materialisation requires an explicit ordering. Stable pagination and `first` require a total ordering, either statically proven or completed with a unique tie-breaker.

Then:

```text
ORDER BY created_at
```

is valid for ordinary `many`, but:

```text
ORDER BY created_at, id
```

is required for cursor pagination or deterministic `first`.

## Do not silently order by primary key

It is tempting to make:

```text
query users
```

implicitly become:

```text
order users by primary key
```

I would avoid that.

It creates hidden semantics:

- tables may have compound keys;
- derived relations may have no key;
- joins and aggregates may not preserve a meaningful key;
- it adds potentially expensive sorts invisibly;
- users will not know why query plans contain ordering;
- changing schema constraints could alter query behaviour.

If RAD wants deterministic output, requiring the frontend to state the order is cleaner than inventing one.

A higher-level frontend can still offer ergonomic sugar:

```go
client.Users.Many(...)
```

could generate primary-key ordering by convention, but that would be a frontend policy made visible in generated LIR, not hidden engine behaviour.

## Orderedness should be a derived property

I would not put an `ordered: true` flag on relations.

The binder/planner can derive order properties:

```text
scan       -> unordered
filter     -> preserves input order
project    -> preserves input order
order      -> establishes order
slice      -> preserves order
aggregate  -> destroys order
join       -> destroys order unless a particular physical promise exists,
              which should not affect logical semantics
union      -> destroys order
```

Then logical validation can ask:

```text
Does this root/crossing require ordered input?
Does its input relation have a logical ordering property?
```

This is similar to type/cardinality analysis.

## My preferred RAD rule

I would adopt something close to:

> LIR relations are unordered unless an `order` operator establishes a logical ordering. Ordering is required whenever row position affects the result or an ordered collection crosses the query boundary.

Concretely:

- `exists`: no order required.
- `scalar`: no order required if statically at most one row.
- `exactly_one`: no order required.
- global aggregate: no order required.
- `first`: order required unless at most one row.
- `array`: order required.
- `slice.offset > 0`: order required.
- `slice.limit`: order required for deterministic selection.
- root `many`: order required if RAD intentionally guarantees deterministic collection output.

I lean toward requiring it for root `many`. It is exactly the kind of mild inconvenience that prevents a large class of subtle bugs.

And importantly, it does **not** mean relations themselves become ordered. It means:

> When you ask RAD to turn a relation into an observable sequence, you must explain how that sequence is ordered.

That is a clean, principled rule.
