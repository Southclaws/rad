# Relation bindings: derived relations as first-class relational values

Status: **complete** (2026-07-13) — all eight steps of the implementation
order resolved. Steps 1–6: schema, forest preflight, binder (canonical
slots, closed bindings, sensitivity, occurrence remapping, unique-output
contract), planner (BindingPlan + RefExec), executor commitment, full
acceptance suite. Step 7 (strategy): a binding with exactly one occurrence
streams inline (replay — its single evaluation is the commitment, so even
sensitive bodies qualify); multi-reference bindings materialise once, which
also keeps nested multi-reference bindings linear at execution where
naive replay would re-run children exponentially. Strategy is chosen by the
planner, shown in EXPLAIN, and pinned by op-count tests (two occurrences
cost one evaluation). Multi-reference replay of insensitive bodies remains
a documented future refinement. Step 8 (correlated bindings): resolved
structurally — document-level bindings have no enclosing scopes at the
binding site, so closedness is a theorem of the decided surface, not a
restriction; per-environment commitment semantics transfer to a future
parameterised construct if one is designed. References inside correlated
contexts (a committed subset consulted per outer row) work today and are
tested. This was the only grammar change to come out of the battle-test
campaign (F7 in tasks/3-done/lir-improvements.md).

The question this document answers is not "should LIR allow DAGs?" (it
should not, and does not — see the preflight rules below). It is: **should
a derived relation be nameable as a first-class relational value**, the way
a base table already is? One model then explains CTEs, repeated
subqueries, nondeterministic choice, self-joins, planner freedom,
materialisation, and lexical correlation together. The construct adds no
new relational operation — it adds the ability to *name a relational
value*, which is why it fits the four-primitive discipline rather than
straining it.

## The problem

LIR is a strict single-consumer tree: a node id names one inline
definition, consumed exactly once, carrying no sharing identity. A query
that uses the same *derived* relation in two sibling contexts must write
the subtree twice. The canonical shape is a CTE self-join:

```sql
WITH expensive AS (
  SELECT user_id, SUM(amount) AS total FROM orders GROUP BY user_id
)
SELECT * FROM expensive a JOIN expensive b ON a.user_id = b.user_id;
```

Today a compiler lowering this must emit the aggregate subtree twice under
fresh ids and scopes. The same pressure appears whenever one derived
relation feeds two crossings (a nested array of it and a fold over it,
side by side).

What this costs:

- **Compilers own a duplication pass.** A SQL frontend must clone subtrees
  and rename scopes instead of emitting one definition and referencing it.
- **Meaning, not just bytes.** For nondeterministic bodies, duplication
  does not even preserve semantics — see below. There are queries LIR
  currently cannot express at all.
- **Physical waste.** Duplicated subtrees are independently planned and
  executed; there is no seam for materialise-once.

Explicitly *not* the problem: "order by a computed value and also return
it." That is a column-level dependency inside one linear pipeline, and its
proper expression is the labelled projection (compute the column once,
reference it above) — documented on the LIR docs page and exercised by the
corpus. Bindings are about sibling contexts only.

## The model: definition, value, occurrence

Base tables already exhibit the complete structure this design needs:

| level      | base table                      | binding (proposed)            |
|------------|--------------------------------|-------------------------------|
| definition | the catalog entry              | the bound relation subtree    |
| value      | one bag, pinned by the snapshot | one bag, committed at binding |
| occurrence | `scan(orders, "a")`, `scan(orders, "b")` — fresh scope+slots each | one `ref` per use — fresh scope+slots each |

A binding makes a derived relation behave exactly like a base table for
the remainder of the statement. Two scans of `orders` are two variables
ranging over one snapshot-pinned value; two references to a binding are
two variables ranging over one committed value. Nobody considers two scans
of the same table a DAG — this construct is no more a DAG than that.

## The normative kernel

The whole semantics in four sentences — the paragraph the eventual spec
section grows from:

> A binding denotes one statement-local relational value (one value per
> instantiation of its lexical environment, if correlated bindings are
> admitted). Every reference observes that same bag of rows and values.
> Each reference exposes it under a fresh local alias. The physical
> planner may evaluate that value by inlining, duplicating, streaming,
> caching, or materialising the binding — anywhere doing so is
> observationally indistinguishable.

The parenthetical in the first sentence tracks open question #1: if the
surface is document-level bindings (no enclosing scopes visible at the
binding site), every binding is closed and "statement-local" is exact with
no parenthetical needed. The verbs are load-bearing: the first three
sentences are purely denotational — nothing executes — and evaluation
appears only in the planner sentence, as a choice among strategies. That
sentence states the criterion, not a mechanism: materialise-once and
identical-plan replay (below) are the two known ways to discharge it, not
its definition.

## Settled semantics

**1. References instantiate.** Each occurrence gets a fresh scope label
and fresh output slots. This is forced by the self-join: `a.user_id` and
`b.user_id` must be distinct bindings; literally shared output slots
cannot express a self-join at all. Binder cost is per-occurrence × output
width (mint slots for the binding's output row type, plus an
occurrence→binding renaming) — the subtree binds once. Wire-linear input
stays binder-linear, even under nested bindings.

**2. A binding is a choice point.** LIR deliberately permits
declared-arbitrary relations: `slice(scan(events), limit: 100)` with no
order denotes "some hundred events" — a *set* of valid bags, not one bag.
A binding commits to one member of that set; every occurrence observes the
commitment. This is what makes the construct semantics-preserving beyond
the deterministic fragment: expanding `let top = any-100(events)` into two
copies silently changes its meaning to two independent draws whose
"self-join" has no diagonal and whose fold need not agree with its rows.

**3. Duplication stays legal and means what it says.** Writing a subtree
twice remains two independent draws. Reference twice for one choice; write
twice for two. Both meanings expressible, each spelled honestly.

**4. Correlated bindings commit per environment.** A binding whose body
references enclosing scopes (lexically closed — free references resolve at
the binding site or the query is ill-formed) denotes one committed value
*per outer environment*. This generalises semantics the engine already
implements: the attach machinery's per-DISTINCT-key deduplication is
compute-once-per-environment for correlated crossings.

**5. The planner rule.** *Commit-once* is the semantic obligation — not
evaluate-once, not materialise-once. Every occurrence must observe the
binding's single committed relational value; how many times anything
physically runs is invisible. Then:

- materialisation preserves the commitment directly;
- identical-plan replay is valid because replay determinism reproduces the
  commitment (physically multiple computations, one committed value);
- divergent planning of a plan-choice-sensitive body is invalid because a
  different plan may select a different legal bag — a different
  commitment.

Bodies that are not plan-choice-sensitive keep full per-occurrence
planning freedom (path independence guarantees any two correct plans
agree). Plan-replay is sound because of **replay determinism**, a stated
and tested engine invariant: the same query against the same statement
snapshot produces the exact same result — same rows and multiplicities,
same chosen arbitrary subsets, same observable order
(`TestReplayDeterminism` in rad/engine/05_exec, which deliberately
includes plan-choice-sensitive shapes; documented in the executor layer
docs, home/content/docs/engine/05-exec.mdx).

**Plan-choice sensitivity** is the property the implementation must derive
and propagate. The question is never "is this relation nondeterministic?"
(the executor is deterministic) but: *can two valid physical plans select
different legal bags?* A bare scan is path-independent as a bag — its
encounter order is arbitrary but unobserved. `slice(scan, limit: 100)`
converts arbitrary order into arbitrary *membership*: sensitive. An order
over a keyless output plus a limit is sensitive at the tie boundary.
The property propagates structurally, and the initial classifier may be
conservative — mark a body sensitive whenever it contains a selection
whose membership is not uniquely determined (a slice not above a proven
total order) — trading optimisation freedom for correctness.

**6. Bindings are an abstraction boundary.** A reference exposes the
binding's *declared output* under the reference's own alias — exactly
`ref expensive AS e`, mirroring `scan(orders, "a")`. Interior scopes never
leak: a body built as `scan users → join orders → aggregate` exposes its
aggregate output, not `u`, `o`, and the intermediates. And the boundary is
a contract:

> A binding's public output schema is fixed at its definition. Changing
> the internal implementation without changing the output schema does not
> affect referring queries.

The public contract is (decided in review): **column names, types, and
nullability** — with declared key/uniqueness properties as a possible
later addition. References are **uniformly 0..many**: static cardinality
remains a property of the actual body and does not become a user-declared
promise, so determinism-sensitive consumers (`first`, `scalar`) of a
reference bring their own order or slice. Rationale: a binding lives
inside the same query document — its body is not truly private from the
statement author or compiler, so the boundary means scope and row-shape
encapsulation, not separate compilation. If bindings ever become
catalog-stored views or reusable prepared modules, declared cardinality
contracts become worth revisiting.

## What does not change

- Ordinary nodes remain strictly single-consumer. The preflight's one-walk
  totality (cycles, sharing, dangling, unreachable), derived correlation
  via free slots, and ids-as-labels are load-bearing and untouched except
  for the explicit, checkable carve-out that refs create.
- Four primitives. A binding is not a relation operator; it is a naming
  discipline over relations, justified precisely because "could this be an
  ordinary relation?" answers: it behaves like the most ordinary relation
  there is — a base table.
- No recursion. Binding references may not form cycles; recursive queries
  are a future construct with their own rules (see the recursion sketch in
  tasks/1-todo/schema-flexibility.md).
- No capture-at-reference. A binding referenced under two different outer
  environments does not re-capture per site; it is lexically closed.
  Parameterised relations (explicit named parameters — relation-valued
  functions) are a separate future construct if wanted, likely designed
  alongside `apply`.

## Decided in review (2026-07-13)

1. **Surface: document-level bindings that name roots within `nodes`.**
   A `let` relation operator was rejected — it would mix naming/environment
   structure into the operator tree; a document-level map establishes the
   second namespace and preserves the statement-wide base-table analogy.
   Bindings do not contain inline bodies; they *identify a root* among
   ordinary nodes, preserving one encoding model for relation trees:

   ```yaml
   bindings:
     expensive: { node: aggregate_expensive }
   nodes:
     scan_orders:         { kind: scan, table: orders, scope: o }
     aggregate_expensive: { kind: aggregate, input: scan_orders, scope: e, ... }
     a:   { kind: ref, binding: expensive, scope: a }
     b:   { kind: ref, binding: expensive, scope: b }
     j:   { kind: join, left: a, right: b, on: { eq a.user_id b.user_id } }
   root: { node: j, cardinality: many }
   ```

   The preflight views the document as a **forest**: one tree rooted at
   `root.node`, one tree per `bindings.*.node`. Ordinary relation edges
   remain tree-shaped; references are edges into the binding namespace,
   never node-consumer edges. Reject: cycles through binding references,
   refs to unknown bindings, unused bindings (unreachable-definition rule
   extended over the forest). Binding names, node ids, and scope labels are
   three separate namespaces; if name/id collisions are rejected, that is a
   readability diagnostic, not a semantic invariant. Design the surface
   alongside the adjacent family (`apply`, `recursive`/`recursive_ref`,
   `union` — schema-flexibility.md), which wants the same reference
   machinery.
2. **Cardinality contract: uniform 0..many references** (folded into
   settled rule 6 above).

## Remaining implementation questions

1. **Binder mechanics.** Representing `BoundBinding` (body, output schema,
   free slots/capture, plan-choice sensitivity) and `BoundRef` (binding
   identity, fresh occurrence slots, canonical-output → occurrence-slot
   mapping); how a correlated binding's free slots interact with the
   environment at each ref.
2. **Physical seam.** How the plan represents the committed choice — an
   explicit materialise/spool operator, an attach-like spec, or plan-replay
   annotation — and what EXPLAIN shows (the binding once, occurrences as
   references).

## Acceptance criteria (tests that must exist before this ships)

- **Occurrence consistency**: `let top = any-100(events)`, self-join on
  the full primary key → exactly 100 rows, all diagonal. The old
  duplication spelling of the same shape keeps its independent-draws
  behavior.
- **Correlated commitment**: a correlated nondeterministic binding
  referenced by two projection fields agrees with itself within every
  outer row.
- **Path independence, extended**: for deterministic bodies, forcing
  different physical strategies per occurrence (materialise vs replay vs
  full-scan) yields identical results; for nondeterministic bodies, the
  planner provably emits one strategy.
- **Cost shape**: nested bindings, each referenced twice, depth ~10 —
  binder and planner stay linear in wire size (the anti-exponential
  guarantee).
- **Alias isolation**: self-join a deterministic binding whose occurrences
  project identically named columns; slots and null-extension behavior
  stay independent per occurrence — catches accidental reuse of canonical
  binding slots instead of proper occurrence remapping.
- **Hidden-scope rejection**: referencing a scope internal to the binding
  body from outside fails to bind; the output alias works — locks the
  abstraction boundary.
- **Preflight probes**: cyclic binding, dangling ref, unused binding,
  ordinary-node sharing (still rejected), binding/node id collision.
- **EXPLAIN**: one definition, N occurrence references, strategy visible.

## Implementation order

1. Wire schema: document-level `bindings` + `RefNode` (lir.schema.yaml →
   schemagen → lirwire).
2. Preflight: validate the forest and the binding dependency graph.
3. Binder: bind each definition once into canonical output slots; derive
   plan-choice sensitivity conservatively.
4. Binder: bind each ref as fresh slots plus canonical→occurrence
   remapping.
5. Planner/executor: lower every binding to explicit materialisation
   first — the clearest correctness oracle.
6. Land the full semantic acceptance suite against materialisation.
7. Add identical-plan replay as an optimisation, checked against the
   materialisation oracle.
8. Correlated bindings after closed bindings work, unless the first
   implementation finds correlation essential.

## Decision record

2026-07-13 design discussion. Considered and rejected: fresh-occurrence
(macro) reference semantics — sound only on the deterministic fragment,
and LIR is deliberately not confined to it; a "self-join" of a
nondeterministic binding would not be a self-join, materialise-vs-recompute
would become observable (breaking path independence), and the conformance
oracle could not pin results. Also rejected: shared output slots across
occurrences (cannot express self-joins), capture-at-reference (an implicit
lambda; parameterisation should be explicit if ever added), and interior
scope leakage through references (destroys the abstraction boundary).

The discussion also reframed the question itself: this began as "should we
allow DAG sharing?" and resolved into "should derived relations be
first-class relational values?" — the title question. The DAG framing was
retired because references are not ordinary node edges at all; they are
edges into the binding namespace, and the node graph stays a tree.
