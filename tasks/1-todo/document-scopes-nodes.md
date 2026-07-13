## “Single-consumer relation tree” means this

You have a flat map:

```yaml
nodes:
  users:
    kind: scan
    table: users
    scope: u

  active_users:
    kind: filter
    input: users
    predicate: ...

  named_users:
    kind: project
    input: users
    fields: ...
```

Here, `users` is consumed twice:

```text
users -> active_users
users -> named_users
```

That makes the structure a **DAG**, not a tree.

A **single-consumer tree** means every node definition is consumed by at most one parent node, except the root selection. So this is okay:

```text
scan -> filter -> project -> root
```

But this is not:

```text
        -> filter_a
scan ->
        -> filter_b
```

Unless you duplicate the scan as two distinct node definitions:

```yaml
users_for_active:
  kind: scan
  table: users
  scope: u

users_for_named:
  kind: scan
  table: users
  scope: u
```

Even though both scans read the same table, they are two separate logical occurrences.

That is what “single-consumer” means.

---

## Can JSON Schema enforce that?

Not really, not with standard JSON Schema.

JSON Schema can validate local object shape:

```yaml
this node has kind/input/predicate
this field is a string
this enum value is valid
```

But it cannot conveniently validate graph properties like:

- every referenced node exists;
- the root node exists;
- there are no cycles;
- every non-root node is reachable;
- no node has more than one consumer;
- crossings only reference valid nodes;
- node references obey scope/correlation rules.

Those are binder/validator responsibilities.

You could make single-consumer enforceable by changing the wire shape to an inline recursive tree:

```yaml
{
  "kind": "filter",
  "input": { "kind": "scan", "table": "users", "scope": "u" },
  "predicate": ...,
}
```

Then a node literally cannot be reused because it is structurally nested.

But I would not necessarily do that. The flat `nodes` map is nicer for:

- code generation;
- debugging;
- deterministic node IDs;
- error messages;
- validation diagnostics;
- planner rewrites;
- frontend emission.

So I would keep the flat encoding and enforce graph validity in the semantic validator.

A good validator pass would compute something like:

```text
reference_count[node_id]

root.node increments count
each input/left/right/node reference increments count

then:
- every reference must point to an existing node
- every node except root-reachable nodes must be rejected as dead code
- every node must have reference_count <= 1
- graph must be acyclic, except future explicit recursive constructs
```

For now, this is better as code than schema.

---

## Why “IDs carry no sharing identity” matters

This means the node ID is just a **name for an inline definition**, not a semantic object identity.

In other words:

```yaml
u:
  kind: scan
  table: users
  scope: user
```

The ID `u` does not mean:

> this is a reusable relation value that can be referenced from many places and shared.

It means:

> here is the definition of this one occurrence in the relation tree.

That distinction matters because if IDs imply sharing, then this:

```text
       -> filter active
scan u
       -> filter admins
```

starts raising awkward questions:

- Is `scan u` executed once and reused?
- Is it a common subexpression?
- Does it have one scope environment or two?
- If it is correlated, correlated against which outer row?
- Can one consumer reorder/filter/project it independently of the other?
- If one branch requires columns pruned, does that affect the other branch?

For a logical IR, you do not want those questions at the wire level.

You want this instead:

```text
Each occurrence of a relation is its own logical node.
If two branches happen to be identical, the optimiser may later detect and share them.
But the wire format does not promise sharing.
```

So “no sharing identity” keeps the LIR simple and referentially boring.

The node ID is a label, not a pointer to a reusable relational object.

That is especially important once you have correlation.

---

## Why normal join should not allow right side to reference left side

Because that changes the meaning of the operator.

A normal join is between two independent input relations:

```text
join(left, right, on)
```

Conceptually:

```text
for rows in left × right:
  keep row pairs where on(row_left, row_right)
```

The right input can be planned independently from the left input.

Example:

```yaml
join:
  left: users
  right: orders
  on: users.id = orders.user_id
```

Both `users` and `orders` are standalone relations. The join condition relates them.

A lateral/apply join is different:

```text
for each row in left:
  evaluate right using that left row
```

Example:

```sql
SELECT *
FROM users u
LEFT JOIN LATERAL (
  SELECT *
  FROM orders o
  WHERE o.user_id = u.id
  ORDER BY o.created_at DESC
  LIMIT 1
) latest_order ON true
```

The right side cannot even be evaluated without a current `u` row.

That is not a normal join. That is an `apply`.

So this should probably be illegal for normal `join`:

```text
right input references left scope
```

Because then your supposedly ordinary join secretly becomes a correlated per-row operation.

Why that matters:

1. **Optimisation is different.**
   Normal joins can be reordered, pushed down, hash-joined, merge-joined, etc. Lateral/apply joins are dependency-sensitive.

2. **Execution is different.**
   A normal right input can be scanned once. A lateral right input may need to be evaluated per left row, or decorrelated into a better plan.

3. **Scope rules stay clean.**
   With normal join, left and right are siblings. Siblings should not see each other. Only the `on` expression sees both.

4. **Future decorrelation gets a clear target.**
   If you add `apply`, the planner can have a specific rewrite rule:

   ```text
   apply(left, correlated_right) -> join(left, decorrelated_right)
   ```

5. **It prevents accidental expensive queries.**
   A frontend author might accidentally reference an outer scope inside the right input and unknowingly turn a cheap join into a per-row nested execution.

So I would keep:

```text
join  = independent inputs, both visible only in `on`
apply = right input may depend on left input
```

That is a very clean boundary.

---

## “A crossing node may reference the current expression environment as outer scope”

This is the correlated subquery rule.

Example:

```yaml
users:
  kind: scan
  table: users
  scope: u

orders_for_user:
  kind: filter
  input: orders
  predicate:
    kind: binary
    op: eq
    left: { kind: col, scope: o, column: user_id }
    right: { kind: col, scope: u, column: id }

users_with_has_orders:
  kind: project
  input: users
  scope: out
  spread: [u]
  fields:
    - as: has_orders
      expr:
        kind: exists
        node: orders_for_user
```

The `exists` expression appears while evaluating each `u` row.

Inside `orders_for_user`, the predicate references:

```yaml
{ kind: col, scope: u, column: id }
```

But `u` is not produced by `orders_for_user`. It comes from the **outer expression environment**.

That means:

```text
for each user row u:
  evaluate whether there exists an order o where o.user_id = u.id
```

That is a correlated crossing.

In SQL terms:

```sql
SELECT
  u.*,
  EXISTS (
    SELECT 1
    FROM orders o
    WHERE o.user_id = u.id
  ) AS has_orders
FROM users u
```

So no, this is not a limitation. It is a very worthwhile constraint.

The constraint is:

> Relation-to-scalar correlation is only allowed through explicit crossing expressions.

That means scalar subqueries are controlled. You know exactly where a relation is being pulled into scalar space.

Without that rule, any random relation node could reference arbitrary outer scopes and you would lose locality.

---

## What this means in reality

This rule allows useful correlated queries:

```text
project users with:
  exists(orders where orders.user_id = current user.id)
```

```text
project boards with:
  array(tasks where tasks.board_id = current board.id)
```

```text
filter users where:
  scalar(count orders for current user) > 10
```

But it prevents this kind of uncontrolled leakage:

```text
some deeply nested scan/filter references whatever outer scope happens to exist
without being under an explicit relation-to-scalar crossing or apply node
```

So the IR stays structured.

You can look at a query and say:

```text
These are relation operators.
These are scalar expressions.
These exact expressions cross from scalar context into relation context.
These are the places correlation may occur.
```

That is extremely valuable for a planner.

---

## The simplest mental model

I would define LIR environments like this:

### Relation nodes produce scopes

```text
scan users as u
=> relation with scope u
```

### Scalar expressions evaluate inside an environment

A filter predicate over `users` has access to `u`:

```text
filter users where u.name = "Barnaby"
```

### Crossings create a nested relation environment that may see the outer scalar environment

```text
exists(
  orders where orders.user_id = outer u.id
)
```

### Normal joins do not create outer environments for their children

This is illegal:

```text
join(
  left: users as u,
  right: orders filtered by u.id
)
```

Instead, use:

```text
join(
  left: users as u,
  right: orders as o,
  on: o.user_id = u.id
)
```

Or later:

```text
apply(
  left: users as u,
  right: latest orders filtered by u.id
)
```

---

## What I’d write into the spec

Something like this:

```text
A Query is encoded as a flat node map, but semantically represents a finite
single-consumer relation tree. Node IDs are definition labels only and do not
represent shareable relation identity.

Each node may be referenced by at most one consuming edge, counting root
selection, relation inputs, and crossing expressions. Reusing a logical
subtree requires duplicating its node definitions. Common-subexpression sharing
is an optimiser concern, not LIR wire semantics.

A relation node is evaluated in a relation environment. Each node exposes a
set of output scopes. Scalar expressions are evaluated against the scopes
visible at their containing operator.

Crossing expressions evaluate a referenced relation in a nested environment
which may capture scopes from the containing scalar expression. Crossings are
the only implicit correlation boundary in this LIR version.

Join children are independent. The left and right input subtrees of a normal
join may not reference each other’s scopes. The join condition is evaluated in
an environment containing both left and right output scopes. Dependent joins
must be represented by a future apply/lateral operator.
```

That paragraph would remove a lot of ambiguity.

The key design principle is: **scope flows downward structurally, and correlation only happens at explicit boundary nodes.**
