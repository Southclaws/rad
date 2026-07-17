# Recursive queries: recursively-defined bindings

Status: **implemented** (2026-07-17), v0 scope. This supersedes
the earlier worked-example sketch on this file and the `RecursiveNode` /
`RecursiveRefNode` proposal in `schema-flexibility.md` ("## Recursive
queries"). The motivating workload is unchanged — a self-referential
comment/thread tree — but the design is now grounded in the LIR that actually
shipped (`bindings` + `ref`, the forest preflight, dense slots) rather than
the older client-side sketch.

What landed, layer by layer: wire `Binding` became a `kind`-tagged union
(`derived` | `recursive`) plus a `recursive_ref` node
(`rad/protocol/lir.schema.yaml`, regenerated `lirwire`); the unbound IR grew
`lir.Recursive` (a binding-body node) and `lir.RecursiveRef`, bridged by
`graphconv`/`wirequery`; the bound IR grew `bound.RecursiveRef` and recursion
fields on `bound.Binding`; the binder's `bindingOrder` treats the `recursive_ref`
self-edge as a non-dependency (the sanctioned cycle), binds anchor → provisional
signature → step → reconcile (kinds equal, nullability join), and enforces the
monotonicity/linearity well-formedness rules; the planner always materialises a
recursive binding and plans anchor + step (`RecursiveRefExec`); the executor and
the `refexec` oracle both run the semi-naive fixpoint with a canonical-slot
projection, an admit-new `bound.CanonicalRowSet` seen-set (the shared full-row
identity the unary `distinct` operator also uses), and iteration/row caps
(`reject.ReasonRecursionLimit`). Tests: `tests/planner/recursive_test.go`
(reachability, depth, the cap, and six well-formedness rejections, all through
the real client→server→bind→plan→execute path) and the e2e fixtures
`tests/e2e/recursive_reachability` and `tests/e2e/recursive_depth`. `task test`
and `task vet` are green.

## The question

Not "should LIR grow cyclic node graphs?" It should not, and does not: the
node forest stays acyclic and single-consumer. The question is the one
`relation-bindings.md` already answered for sharing, asked one step further:
**a recursive CTE is a recursively-defined named relation**, and LIR already
models named relational values as bindings. Recursion is therefore *the one
sanctioned cycle in the binding-dependency graph*, reached only through a
frontier reference — the next checkable carve-out after ordinary `ref`, not a
new graph regime. It adds no relational algebra beyond a fixpoint.

`bind.go`'s `bindingOrder` already topologically sorts bindings and rejects
back-edges with "binding %q is part of a binding cycle" (`bind.go:237`). That
rejection *is* the current recursion prohibition; this design relaxes it for
exactly one shape.

## The model: recursion is a kind of binding

Recursion lives on the binding, not on a relation node. A `RecursiveNode` that
names a binding would invert ownership and mix two namespaces; a binding
already owns its definition, so it owns the recursive definition too.

- **Derived binding** (today) — `{ node }`: one defining root; one committed
  value.
- **Recursive binding** (new) — `{ kind: recursive, anchor, step, accumulation }`:
  an anchor root (the base case) and a step root (the inductive case),
  combined by accumulation semantics into a fixpoint value.
- **`recursive_ref`** (new node) — `{ binding, scope }`: a relation leaf,
  legal *only* inside its own binding's `step`, denoting the **previous
  iteration's frontier** (working table), exposed under a fresh scope exactly
  as `ref` exposes a committed binding.
- **`ref`** (today, unchanged) — observes the *completed*, post-fixpoint value.
  This is how the root reads the recursive binding.

The two references are deliberately distinct nodes, not one `ref` with a flag:
they have different legality (a `recursive_ref` is confined to its step) and
different denotation (frontier vs. committed value), and a distinct `kind` is
how this IR already separates constructs whose validity differs.

## The normative kernel

Semi-naive evaluation, stated denotationally (the paragraph a spec section
grows from):

> A recursive binding is evaluated by iterating a step over a frontier.
> Evaluate the `anchor` once; its rows seed both the result and the working
> table. Then repeat: evaluate `step` with `recursive_ref` bound to the current
> working table; the rows produced become the next working table and are added
> to the result; stop when the working table is empty. Under `accumulation: all` every
> produced row enters the result and the next working table, with its
> multiplicity. Under `accumulation: new` the value is the least fixpoint of a
> set: a produced row is admitted at most once — dropped if it is already in
> the accumulated result or a duplicate of another row produced in the same
> iteration — and the survivors form the next working table. `recursive_ref`
> denotes the working table — the previous iteration's rows — never the
> accumulated result. The value has no inherent order; observing it requires an
> explicit `order`, exactly as for any relation.

Frontier semantics are not an optimisation; they are the meaning. If
`recursive_ref` exposed the whole accumulated result, every iteration would
re-expand every row found so far — under `accumulation: all` that reproduces old rows
forever even on an acyclic tree.

## Settled semantics

1. **`recursive_ref` is the frontier**, not the accumulation. At iteration 0
   the step sees the anchor's rows; at iteration 1 their successors; and so on.

2. **Accumulation is explicit.** `accumulation: all` keeps every derived row with its
   multiplicity; `accumulation: new` admits each full row at most once — removing
   rows already in the accumulated result *and* duplicates produced within the
   same iteration — before forming the next frontier. Row identity under
   `accumulation: new` is *full-row* equality with `NULL == NULL` (a canonical
   row-key encoding), not K3 predicate equality where `NULL ≠ NULL`. This
   admit-new accumulation and the unary `distinct` relation operator are
   separate logical concepts: admit-new incrementally rejects rows already in
   the fixpoint result, while `distinct` deduplicates one completed arbitrary
   relation. They share the canonical full-row identity (`bound.CanonicalRowSet`)
   but remain different LIR constructs and planner operations.

3. **Termination is the author's responsibility; the engine only backstops
   it.** Frontier evaluation alone does not guarantee termination: under
   `accumulation: all`, `A.parent = B, B.parent = A` yields frontiers `A, B, A, B, …`.
   `accumulation: new` terminates only when the reachable *result* is finite — and a
   monotonically growing column (a `depth`, a `path`) makes every revisit a
   distinct row, so `accumulation: new` does not save it. Path-sensitive traversals still
   need an explicit guard (`not contains(parent.path, child.id)` once arrays
   exist). Independently, the executor enforces maximum iteration and
   accumulated-row caps, failing the query with a stable `reason` when
   exceeded. These are execution safeguards, never logical semantics. The
   accumulated-row cap bounds admitted output but not wasted work: a
   duplicate-heavy `accumulation: new` step can generate many candidate rows while
   admitting few, so a separate generated-row/work cap is a likely later
   addition — an executor detail, not a wire-semantic change.

4. **The anchor supplies the signature; nullability is a fixpoint.** The
   anchor's output names, order, and kinds *are* the binding's public shape.
   Bind the step against the current signature, then join the anchor and step
   nullability (a column is nullable if either makes it so) — and re-bind the
   step against the widened signature until it stops changing. One pass is not
   enough: a step column derived from another (`b = parent.a`) only learns its
   true nullability once the column it tracks has widened, so nullability
   propagates one dependency edge per pass. Kinds must stay equal (mismatch is a
   bind error) and only nullability moves, over a finite two-point lattice, so
   the loop terminates in at most one pass per column. No declared `columns`
   block — the "derived, never asserted" invariant holds. Because `LiteralExpr`
   carries a typed `Value` on the wire, `depth = 0` is a typed `int64` literal
   directly: no cast, no signature declaration. (`bindRecursiveBinding` in
   `04_planner/bind.go` re-binds the step from a fixed slot/label mark each
   pass, so the final slots stay deterministic.)

5. **Well-formedness is a conservative monotone fragment** (see below). The v0
   rules structurally guarantee the step is monotone in the recursive relation
   — adding frontier rows never retracts output — which is what makes iterative
   accumulation semantically stable. Monotonicity is *not* a termination or
   finite-fixpoint guarantee (see 3), and the rules are deliberately sufficient
   rather than complete: some monotone recursions (certain aggregates) are
   excluded because admitting them needs a richer semantic model, not because
   they are unsound.

6. **Traversal order is never evaluation order.** The recursive relation has no
   inherent order; the fixpoint's visiting sequence is unobservable. Any
   ordered result comes from an ordinary `order` over the completed value —
   the same determinism rule as everywhere in LIR.

7. **A recursive binding is an abstraction boundary**, like a derived one. Both
   `ref` and `recursive_ref` expose the binding's public columns under a fresh
   occurrence scope with fresh slots (reusing `bindRef` / `remapOccurrence`);
   interior scopes never leak.

8. **Recursion affects membership, not nesting.** The completed binding is an
   ordinary flat relation; downstream projections may attach nested values
   through normal crossings, but parent/child JSON tree construction is a
   separate shaping concern, never part of recursion itself.

## Well-formedness (validator rules)

The step must be **monotone** in the recursive relation (more input rows can
only produce more output rows). The v0 rules are a *conservative* monotone
fragment — structurally checkable and sufficient, not a characterisation of
every monotone query:

- **Linear recursion:** the step contains **exactly one** `recursive_ref`; the
  anchor contains **none**. (SQL and Postgres both require exactly one; more
  than one is a product of frontiers — a heavier fixpoint deferred past v0.)
- **`recursive_ref` placement:** only as an ordinary relational input to the
  `filter`/`join`/`project` pipeline. It may **not** feed (directly or
  transitively) an `aggregate`, sit on the null-extending side of a `left`
  join, appear under a negating crossing (`not exists(… recursive_ref …)`), or
  sit under an `order`/`slice` that bounds membership before accumulation. These
  are the non-monotone positions; they also keep the step plan-choice
  *insensitive*, so per-iteration commitment stays deterministic and replay
  holds.
- **Scope of the reference:** `recursive_ref.binding` must equal the enclosing
  recursive binding. A `recursive_ref` outside a recursive step, an ordinary
  `ref` to a recursive binding from *within* its own anchor or step, and a
  `recursive_ref` targeting a *different* binding (mutual recursion) are all
  rejected in v0.
- **Forest preflight extension:** the binding-dependency graph stays acyclic
  *except* for the single self-edge a recursive binding's step introduces via
  its `recursive_ref`. `bindingOrder` (`bind.go:216-259`) treats a recursive
  binding as one unit and does not count that self-edge as a topo edge, while
  still rejecting mutual cycles and ordinary self-reference.

## Wire shape

`Binding` becomes a `kind`-tagged union, matching how `Node`, `Expr`, and
`Value` already discriminate. `recursive_ref` joins the `Node` union.

```yaml
Binding:
  oneOf: [DerivedBinding, RecursiveBinding]

DerivedBinding:            # today's binding, now tagged
  required: [kind, node]
  properties:
    kind: { const: derived }
    node: { type: string, minLength: 1 }   # the defining root

RecursiveBinding:
  required: [kind, anchor, step, accumulation]
  properties:
    kind:   { const: recursive }
    anchor: { type: string, minLength: 1 }  # base-case root (no recursive_ref)
    step:   { type: string, minLength: 1 }  # inductive root (exactly one recursive_ref)
    accumulation: { enum: [all, new] }

RecursiveRefNode:          # a new member of the Node oneOf
  required: [kind, binding, scope]
  properties:
    kind:    { const: recursive_ref }
    binding: { type: string, minLength: 1 } # must be the enclosing recursive binding
    scope:   { type: string, minLength: 1 } # fresh, unique across the query
```

A general `union` / set-operation node is *not* introduced by this change (see
"Adjacent features"). The recursive binding owns its accumulation semantics directly.

## Worked examples

### A. Reachability — the first fixture (scalar only, no new value model)

Every descendant id of a root post. Uses only `text`, terminates on any finite
graph through `accumulation: new` with no guard, and needs nothing that does not
exist today.

```yaml
nodes:
  a_scan:   { kind: scan, table: posts, scope: pa }
  a_filter: { kind: filter, input: a_scan,
              predicate: { kind: binary, op: eq,
                left:  { kind: col, scope: pa, column: id },
                right: { kind: lit, value: { type: text, value: "post_123" } } } }
  a_proj:   { kind: project, input: a_filter, scope: a,
              fields: [ { as: id, expr: { kind: col, scope: pa, column: id } } ] }

  s_scan:     { kind: scan, table: posts, scope: pc }
  s_frontier: { kind: recursive_ref, binding: descendants, scope: parent }
  s_join:     { kind: join, left: s_scan, right: s_frontier, join: inner,
                on: { kind: binary, op: eq,
                  left:  { kind: col, scope: pc,     column: parent_id },
                  right: { kind: col, scope: parent, column: id } } }
  s_proj:     { kind: project, input: s_join, scope: s,
                fields: [ { as: id, expr: { kind: col, scope: pc, column: id } } ] }

  out:     { kind: ref, binding: descendants, scope: d }
  ordered: { kind: order, input: out,
             terms: [ { expr: { kind: col, scope: d, column: id } } ] }

bindings:
  descendants: { kind: recursive, anchor: a_proj, step: s_proj, accumulation: new }

root: { node: ordered, cardinality: many }
```

### B. Descendants with depth (scalar `int64`, `accumulation: all`, tree-terminating)

Add `depth`. The anchor's `depth` is a typed `int64` literal; the step's is
`parent.depth + 1`. The binding's row type `{ id: text, depth: int64 }` comes
entirely from the anchor. Order by `(depth, id)` — a real, deterministic order
in the current type system. Terminates because a tree is finite (a general DAG
would need `accumulation: new` or a guard).

```yaml
# a_proj.fields:
#   { as: id,    expr: { kind: col, scope: pa, column: id } }
#   { as: depth, expr: { kind: lit, value: { type: int64, value: "0" } } }
# s_proj.fields:
#   { as: id,    expr: { kind: col, scope: pc, column: id } }
#   { as: depth, expr: { kind: binary, op: add,
#                        left:  { kind: col, scope: parent, column: depth },
#                        right: { kind: lit, value: { type: int64, value: "1" } } } }
# bindings.descendants.accumulation: all
# ordered.terms: [ { expr: col d depth }, { expr: col d id } ]
```

### C. The Hacker News thread with `path` — deferred, gated on arrays

The original motivating query (carry `path = array_append(parent.path,
child.id)`, guard with `not contains(parent.path, child.id)`, `ORDER BY path`)
is the eventual target but depends on a value-model feature this change does
*not* deliver (below). Chronological sibling order additionally wants
`timestamp` and a path of sort keys `(created_at, id)`, not raw ids
(lexicographically `"comment_10" < "comment_2"`), which pushes `path` toward
`array<row{created_at, id}>`. Ship it as example three once arrays land.

## Adjacent features this needs (and their boundaries)

- **A general `union` node — recommended, not required, and not part of this
  change.** Recursion carries its own fixpoint machinery regardless, so a
  standalone `union` buys little executor reuse. Its real payoff for recursion
  is structural: with it, a *multi-term* anchor or step is an ordinary `union`
  inside the `anchor`/`step` root, so the recursive binding stays strictly
  single-anchor/single-step forever. Land it first for that cleanliness (and
  for the SQL frontend, which will want `union`/`intersect`/`except` anyway),
  but it is not a blocker for the scalar fixtures.

- **The array value-model — a separate feature, deferred.** `array` exists
  today *only* as a crossing that materialises a relation into a nested datum
  (`CrossingExprArray`); `KindArray` exists for that nested output. There is no
  scalar `array<T>` column type, no array literal from scalars, and no
  `append`/`contains`. Adding those is not three expression variants — by
  relational closure a projected `path: array<text>` is a *real* row-type
  attribute, so it forces decisions about array equality, lexicographic
  ordering, grouping keys, `rows` cells, storage, `accumulation: new` structural
  comparison, and element-vs-array nullability. That is its own coherent piece
  of work; recursion must not smuggle it in. Reachability and depth need none
  of it.

- **`timestamp`** is not in the four-scalar type system (`text`, `int64`,
  `float64`, `bool`); the HN example's `created_at: timestamp` is illustrative
  of a future type, not today's model.

## v0 scope (settled)

**Ships:**

- the recursive `Binding` kind (`anchor` / `step` / `accumulation`) and the
  `recursive_ref` node, with frontier semantics;
- `accumulation: all` **and** `accumulation: new` (admit-new over scalar rows is cheap and
  gives reachability with zero guard logic — the cleanest first proof);
- linear recursion (exactly one `recursive_ref`) plus the monotonicity
  well-formedness checks;
- anchor-derived row type, with the nullability join (no declared columns, no
  recursion-specific literal typing);
- executor max-iteration / max-row caps as a safeguard;
- fixtures: **A (reachability)** first, then **B (depth)**.

**Deferred, each as its own feature:** the array value-model (and with it
example C, `append`/`contains`, `path` ordering); `timestamp`; a general
`union`/set-operation node; non-linear and mutual recursion; SQL-style
`CYCLE` / `SEARCH` sugar.

## Implementation (grounded in the current engine)

The impl is small because the seams already exist; the only genuinely new
machinery is the fixpoint loop and a frontier buffer.

- **Binder** (`04_planner/bind.go`): relax `bindingOrder` for the single
  recursive self-edge (§ well-formedness). Bind a recursive binding by binding
  its `anchor` into canonical output slots (as a derived binding's root is
  bound today), binding the `step`'s `recursive_ref` as an *occurrence* against
  those canonical slots (reuse `bindRef` / `Canon` / `remapOccurrence`,
  `bind.go:501-525`), then binding the `step` and reconciling its output with
  the canonical row type (kinds equal, nullability join). Enforce the
  monotonicity placement rules while walking the step.

- **Planner** (`04_planner/plan_lir.go`): a recursive binding is **always**
  `BindingMaterialise` — never `BindingReplay` — since it must reach fixpoint
  before observation (special-case it in `countRefs`, `plan_lir.go:81-104`).
  Plan the anchor once and the step once; the step's `recursive_ref` plans to a
  read of the frontier buffer, structurally a `RefExec` (`physical.go:194`)
  against a per-binding frontier rather than the committed map.

- **Executor** (`05_exec`): extend `commit` (`operators.go:46-66`). For a
  recursive binding: `WT := drainOp(anchor)`, `result := WT`; loop — publish
  `WT` as the binding's frontier buffer, `new := drainOp(step)`, under
  `accumulation: new` drop rows whose canonical identity is already in a seen-set, `WT :=
  new`, append `WT` to `result`, until `WT` is empty or a cap trips; store
  `result` in `executor.bindings[name]`. Reuse: the `executor.bindings` frame
  map and `drainOp`, `bound.CanonicalRowSet` for the admit-new seen-set, and
  `refOp` / `remapOccurrence` (`operators.go:150-195`) for both reference ops.
  The driver is naturally a blocking operator (compute the whole fixpoint, then
  stream), which the pull model already accommodates.

- **Reference interpreter** (`05_exec/refexec`, the oracle) — trivial: its
  commit loop already fills a `bindings map[string][]bound.Env` that a
  `*bound.Ref` reads (`interp.go:49-94, 142-157`), and `in.rel(root)` already
  returns a fully materialised bag. Add the same anchor/step/frontier loop over
  that map; a `recursive_ref` reads the frontier entry. Land the oracle first —
  it is the correctness spec the physical loop is checked against.

- **Caps:** both interpreters honour the max-iteration and max-row limits,
  failing with a stable `reason`.

## Acceptance criteria

- Reachability (example A) over a fixed tree returns exactly the descendant
  set, order-stable under `order by id`; engine result equals the refexec
  oracle.
- Depth (example B): `accumulation: all` over a tree terminates; each row's `depth` is
  its distance from the root.
- `accumulation: new` de-duplicates by full row with `NULL == NULL`; a diamond DAG
  yields one row per node under `accumulation: new`, and (with a growing `depth` column)
  demonstrably does *not* terminate the depth query without `accumulation: all` + a finite
  tree — i.e. the semantics are pinned, not just the happy path.
- Termination cap: a two-cycle (`A↔B`) under `accumulation: all` fails with the
  iteration-cap `reason`, not a hang.
- Well-formedness rejections, each with a distinct diagnostic: zero or two
  `recursive_ref`s in a step; a `recursive_ref` in the anchor; a
  `recursive_ref` under an `aggregate` or the null side of a `left` join; an
  ordinary `ref` to a recursive binding from inside its own step; a
  `recursive_ref` to another binding; mutual recursion `a → b → a`.
- Abstraction boundary: interior scopes of the anchor/step are not visible
  through the outer `ref`; the public columns are.
- Anchor-signature typing: a step column whose kind differs from the anchor's
  is rejected; a step that adds nullability widens the binding's column to
  nullable.

## Decision record (2026-07-17)

- **Recursion is a binding kind, not a relation node** — the binding owns its
  (recursive) definition; a node must not reach up to declare a binding.
- **`recursive_ref` is the frontier** (semi-naive), correcting the earlier
  "join against everything found so far" framing, which re-expands old rows and
  diverges under `accumulation: all` even on an acyclic tree.
- **Linear recursion only** in v0 (exactly one `recursive_ref`).
- **Ship `accumulation: all` and `accumulation: new`** together; reachability under
  `accumulation: new` is the cheapest end-to-end proof.
- **The anchor is the signature** — no declared `columns` block; kinds unify by
  equality, nullability by join. Self-typed wire literals make even the
  `depth = 0` case need no cast.
- **Termination is the author's responsibility**; engine caps are a backstop,
  not a semantic.
- **Arrays are a separate value-model feature**, not a recursion dependency;
  the scalar fixtures ship first.
- Considered and rejected: a `RecursiveNode(binding, anchor, step)` that names a
  binding (inverts ownership, mixes namespaces); a declared-`columns` block on
  the recursive binding (breaks "derived, never asserted"; the anchor already
  supplies the shape); `recursive_ref` denoting the full accumulated result
  (non-terminating re-expansion); one overloaded `ref` with a frontier flag
  (conflates two different legalities and denotations).

## Semantics suite and findings

`tests/planner/recursive_semantics_test.go` covers recursion by *shape*, not by
example — graph reachability (bag vs. set via a diamond), general DAGs,
two-node and self-loop cycles (termination via cap), empty/missing/multiple
anchors, connected components, ancestor traversal (FK reversed), recursive
state (cost, carried columns), NULL identity under admit-new, recursive joins
(the step joins the frontier and another table), and executor stress (1000-way
fan-out, 500-deep chain). All run through the real client→server path over an
edge table (graphs) or the `shop` referral tree.

Two limitations surfaced, both use-case-driven rather than recursion-specific,
and both are now fixed:

- **Iteration cap vs. legitimate depth (resolved).** The cap was briefly 1000,
  which would reject a genuinely 1000-deep traversal as if it were a runaway.
  The caps are now a configurable engine option — `exec.WithRecursionLimits`
  ({MaxIterations, MaxRows}), defaulting to 10000 iterations / 1,000,000 rows —
  so an operator tunes them to the deepest legitimate hierarchy rather than any
  language property (`TestRecursionCapConfigurable`). A separate
  generated-row/work counter to bound duplicate-heavy `accumulation: new` steps (see
  settled semantic 3) remains a future refinement.
- **Typed NULL literals lose their type in lowering (resolved).**
  `decodeValue` used to lower a wire `{type: text}` NULL to an untyped `nil`,
  relying on the binder to retype from context; a *projected* NULL (a step
  initialising a nullable state column, or `SELECT NULL::text`) has no context
  and was rejected as "a bare NULL literal needs a typed context".
  `lir.Literal` now carries the declared `Kind` for a NULL, threaded through
  `graphconv`, the binder, and `wirequery`, so a projected typed NULL binds
  (`TestProjectedTypedNull`, and `TestRecNullState` for a NULL-initialised
  recursive state column). Non-NULL literals keep their context-typed
  behaviour untouched.

A generative recursive-graph oracle now covers this too. `05_exec/generative/
recursive.go` synthesises a random directed graph and a correct-by-construction
recursive query over it — random anchor set, accumulation mode, and recursive-state
signature (id-only through five typed/nullable columns) — and `tests/gen`'s
`TestGenerativeRecursive` runs each through the three-way differential (engine
chosen plan vs. forced full scan vs. reference interpreter), shrinking any
divergence and, under `RAD_GEN_EMIT`, emitting it as a permanent e2e fixture
(the recursive query round-trips through `api.WireQuery`, the edge schema
through `emit.SchemaRAD`). Termination is guaranteed by construction — acyclic
graphs (edges point forward in node order) carry rich state under either accumulation
mode; arbitrary, possibly-cyclic graphs use `accumulation: new` over id-only rows,
which closes on the finite node set — so every case checks a *result*, not the
iteration cap (cycles and caps stay the hand-written suite's job). Comparison
is a multiset: a recursive relation has no inherent order, and `accumulation: all`
duplicates would make a sequence comparison spuriously fail.

## Architecture note

Recursion could have blown apart the "reuse existing machinery" discipline; it
did not. The whole feature is one new binding kind (`recursive`), one new leaf
node (`recursive_ref`), a binder carve-out (the sanctioned self-edge + the
signature fixpoint), a planner materialise-and-plan-the-step extension, and an
executor fixpoint loop. Scopes, dense slots, occurrence remapping, binding
commitment/materialisation, the reference interpreter, EXPLAIN, and the
three-way differential were all reused unchanged. That the hardest planned
logical operator dropped in this cleanly is a strong validation of the
relation-graph IR and the binding model it extends.

## Related

- `tasks/3-done/relation-bindings.md` — the binding model, occurrence
  remapping, forest preflight, and plan-choice sensitivity this reuses.
- `tasks/1-todo/schema-flexibility.md` — the adjacent `apply`/lateral, `window`,
  and set-operation sketches; recursion shares its reference machinery.
- `tasks/1-todo/next-steps.md` — the roadmap entry that deferred recursion until
  a workload justified it (this is that workload).
- `rad/protocol/lir.schema.yaml` — the wire grammar the two new constructs
  extend.
