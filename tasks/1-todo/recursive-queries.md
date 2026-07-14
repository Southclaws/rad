# Recursive queries: a worked example (Hacker News comment tree)

Status: idea, not scheduled — no LIR spec or implementation yet. Captured
because a concrete workload (self-referential comment/thread trees) is the
kind of motivating case `next-steps.md`'s roadmap wants before recursion
becomes a real operator ("Windows and recursive queries — genuine future
logical operators; defer until a target workload requires them"). This is
that workload.

The design sketch already on file (`schema-flexibility.md`, "## Recursive
queries") proposes the operators at the LIR-node level: `RecursiveNode`
(`binding`, `scope`, `anchor`, `step`, `mode: union_all | union_distinct`)
and `RecursiveRefNode` (`binding`, `scope`), with recursion as the one
sanctioned exception to the acyclic single-consumer graph rule, reached only
through `recursive_ref`. `relation-bindings.md` defers recursive binding
references to that same sketch — recursion is explicitly out of scope for
the (now shipped) non-recursive binding semantics.

This task is a companion to that sketch, worked from the client side: what
would a real caller actually send for a self-referential-tree query, ignoring
current LIR shape entirely? Below is that sketch — a Hacker News–style
post/comment thread, given a root post id, returning every descendant with
depth and a path for ordering. Treat the JSON as illustrative, not
normative; the point is the shape of the problem, not the wire format.

## The query, conceptually

```sql
WITH RECURSIVE thread AS (
  SELECT id, parent_id, author, text, created_at, 0 AS depth, ARRAY[id] AS path
  FROM posts
  WHERE id = $root_id

  UNION ALL

  SELECT child.id, child.parent_id, child.author, child.text, child.created_at,
         parent.depth + 1, array_append(parent.path, child.id)
  FROM posts child
  JOIN thread parent ON child.parent_id = parent.id
)
SELECT * FROM thread ORDER BY path;
```

## The client-side sketch

A recursive binding with one declared output shape, an `anchor` (base case)
and a `step` (recursive case) that must both produce that shape, combined by
`union` semantics, with the accumulated relation addressable inside `step`
via a `recursive_ref`:

```json
{
  "query": {
    "bindings": {
      "thread": {
        "recursive": {
          "columns": {
            "id": "string", "parent_id": "string?", "author": "string",
            "text": "string", "created_at": "timestamp",
            "depth": "int64", "path": "array<string>"
          },
          "anchor": {
            "from": { "table": "posts", "as": "p" },
            "where": { "eq": [{ "column": ["p", "id"] }, { "param": "root_id" }] },
            "select": {
              "id": { "column": ["p", "id"] },
              "parent_id": { "column": ["p", "parent_id"] },
              "author": { "column": ["p", "author"] },
              "text": { "column": ["p", "text"] },
              "created_at": { "column": ["p", "created_at"] },
              "depth": { "literal": 0 },
              "path": { "array": [{ "column": ["p", "id"] }] }
            }
          },
          "step": {
            "from": {
              "join": {
                "left": { "table": "posts", "as": "child" },
                "right": { "recursive_ref": "thread", "as": "parent" },
                "type": "inner",
                "on": { "eq": [{ "column": ["child", "parent_id"] }, { "column": ["parent", "id"] }] }
              }
            },
            "where": {
              "not": { "contains": [{ "column": ["parent", "path"] }, { "column": ["child", "id"] }] }
            },
            "select": {
              "id": { "column": ["child", "id"] },
              "parent_id": { "column": ["child", "parent_id"] },
              "author": { "column": ["child", "author"] },
              "text": { "column": ["child", "text"] },
              "created_at": { "column": ["child", "created_at"] },
              "depth": { "add": [{ "column": ["parent", "depth"] }, { "literal": 1 }] },
              "path": { "append": [{ "column": ["parent", "path"] }, { "column": ["child", "id"] }] }
            }
          },
          "union": "all"
        }
      }
    },
    "root": {
      "from": { "binding": "thread", "as": "thread" },
      "order_by": [{ "expr": { "column": ["thread", "path"] }, "direction": "asc" }],
      "select": { "...": "one field per declared column" }
    }
  },
  "params": { "root_id": "post_123" },
  "cardinality": "many"
}
```

The result stays a flat, ordered relation — one row per post/comment, each
carrying `depth` and `path`:

```json
[
  { "id": "post_123",   "parent_id": null,       "depth": 0, "path": ["post_123"] },
  { "id": "comment_1",  "parent_id": "post_123",  "depth": 1, "path": ["post_123", "comment_1"] },
  { "id": "comment_4",  "parent_id": "comment_1", "depth": 2, "path": ["post_123", "comment_1", "comment_4"] }
]
```

Nesting that into a `children`-shaped tree for display is deliberately a
*separate* concern — a result-shaping feature on top of an ordinary flat
relation (something like `"tree": { "from": ..., "id": ..., "parent": ...,
"children_as": "children" }`), not part of recursion itself. Keep those
independent: recursion stays relational, hierarchy becomes presentation.

## What this pressure-tests in the existing sketch

- **Cycle protection.** The schema forbidding cycles in stored data is not
  the same guarantee as the recursive *query* terminating — a step whose
  join condition can revisit an already-seen row needs either (a) fixpoint
  semantics that only ever add genuinely new rows (frontier/delta-based),
  making the check implicit, or (b) an explicit anti-revisit predicate like
  `not(contains(parent.path, child.id))`, as sketched above. `schema-
  flexibility.md` leaves open whether `recursive_ref` denotes the previous
  iteration's frontier or the full accumulated table; this example wants
  the latter (a join against everything found so far), which is closer to
  standard SQL recursive-CTE "working table" semantics than frontier-only
  tree traversal, and makes explicit cycle protection necessary rather than
  optional.
- **Missing expression primitives.** `array` already exists as a *crossing*
  (materialising a whole relation into one array-valued field), but this
  needs array *construction from scalars* (`{"array": [...]}` from a single
  row's columns) and an array `contains`/`append` predicate/function —
  neither is in `Expr` today. These are ordinary scalar operators, not
  recursion-specific, but recursion is what first needs them.
- **`union` is a prerequisite**, per `schema-flexibility.md`'s own
  recommendation ("plan for union before recursive queries") — the anchor/
  step combination needs `UNION ALL` (and maybe `union distinct`) as a real
  node before `recursive` can be expressed as anything but a special case.
- **Declared output shape up front.** Unlike every other LIR node, the
  binding's `columns` must be declared before either `anchor` or `step` is
  known to type-check against it — anchor and step are checked *against*
  the declaration, not inferred from it. This is a new kind of node in LIR:
  every other node's output shape is derived, not asserted.
- **Ordering.** Same rule as everywhere else in LIR — the recursive
  relation itself has no inherent order (a fixpoint computation visits rows
  in an implementation-defined sequence), so the worked example's `ORDER BY
  path` is not optional decoration; without it, observing the relation would
  depend on the fixpoint's internal visiting order, exactly the invariant
  `document-scopes-nodes.md` already enforces for ordinary relations.

## Non-goals for this task

- Not proposing a wire format — `schema-flexibility.md`'s `RecursiveNode`/
  `RecursiveRefNode` sketch is the closer-to-real starting point; this is
  a worked example to test that sketch against, not a competing proposal.
- Not proposing the `children_as` tree-shaping construct as part of this
  work — noted only to keep it explicitly out of scope.

Related: tasks/1-todo/schema-flexibility.md ("Recursive queries", "Lateral
joins" for the adjacent `apply` operator), tasks/3-done/relation-bindings.md
(binding reference machinery recursion will reuse), tasks/1-todo/next-steps.md
(roadmap entry deferring recursion until a workload justifies it).
