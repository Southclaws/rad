# Can the planner optimise duplicated logical nodes into one physical query?

Yes, but with caveats.

This is exactly where you want the distinction:

```text
LIR wire format: no sharing identity
planner: free to discover sharing
executor: free to reuse physical work
```

So a frontend may emit:

```text
scan users -> filter active
scan users -> filter admins
```

as two separate logical occurrences.

Then the optimiser can notice:

```text
both branches read users
maybe scan users once
maybe push both predicates into one scan
maybe build a shared physical source
```

But the LIR itself should not promise that those two logical occurrences are the same runtime object.

That keeps the logical contract simple.

## Why not allow multi-consumer nodes directly?

You could, but it would make the wire IR more semantically loaded.

This:

```text
        -> filter active
scan u
        -> filter admins
```

would mean `scan u` is now a shared logical value. Then you have to answer:

- Is it legal to consume this from two different correlation environments?
- Is it evaluated once or conceptually duplicated?
- Can each consumer prune different columns?
- Can each consumer impose different ordering requirements?
- What if one consumer sits under a crossing and the other does not?
- Does the shared node carry a single scope identity into both branches?

None of those are unsolvable, but they complicate the IR early.

With single-consumer logical nodes, the answer is boring:

```text
Every node occurrence belongs to exactly one place in the query.
If two occurrences are identical, the planner may optimise them.
```

That is a much nicer foundation.

## Common-subexpression elimination belongs after binding

I would not do CSE on raw JSON.

You want to compare bound, normalised logical plans, not raw node objects.

For example, these may be equivalent after binding:

```yaml
a:
  kind: scan
  table: users
  scope: u
```

```yaml
b:
  kind: scan
  table: users
  scope: user
```

Structurally, the scopes differ. Semantically, depending on context, they may still represent equivalent scans.

But this is not always safe:

```text
orders where orders.user_id = outer_user.id
```

Two such subqueries are only equivalent if they capture the same outer environment.

So the optimiser should compare something like:

```text
operator kind
table/index
predicate
projection requirements
ordering requirements
correlation bindings
required columns
cardinality constraints
```

Not just the raw YAML.

## I’d name these passes explicitly

For RAD, I’d probably use terms like:

```text
Schema validation
  checks the LIR document shape

Logical validation / binding
  resolves nodes, scopes, columns, types, cardinality, and correlation

Logical optimisation
  rewrites the bound tree without changing semantics

Physical planning
  chooses scans, joins, batching, indexes, execution strategy
```

The binder is where the “real query validity” lives.

## The important design principle

The frontend should emit a **clear logical tree**.

The planner may turn it into a **DAG or shared physical plan**.

That is the clean abstraction boundary:

```text
Wire LIR:
  simple, explicit, tree-shaped, no sharing promises

Planner IR:
  free to share, reorder, decorrelate, batch, cache, fuse, split

Physical plan:
  optimised execution strategy
```

So yes: single-consumer LIR plus post-schema logical validation is the right direction. It gives you boring semantics at the protocol layer and leaves cleverness where it belongs: in the planner.
