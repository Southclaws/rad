# Relation bindings: sharing a derived relation across sibling contexts

Status: problem and semantics settled, ready for review. Surface syntax,
binder/preflight mechanics, and the physical seam are the remaining design
work. This is a grammar change to lir.schema.yaml — the only one to come
out of the battle-test campaign (F7 in tasks/3-done/lir-improvements.md) —
and nothing lands until the open questions below are settled with the same
care.

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

> A binding evaluates to one statement-local relational value (one value
> per instantiation of its lexical environment, if correlated bindings are
> admitted). Every reference observes that same bag of rows and values.
> Each reference exposes it under a fresh local alias. The physical
> planner may inline, duplicate, stream, cache, or materialise the binding
> only where doing so is observationally indistinguishable.

The parenthetical in the first sentence tracks open question #1: if the
surface is document-level bindings (no enclosing scopes visible at the
binding site), every binding is closed and "statement-local" is exact with
no parenthetical needed. The fourth sentence is the criterion, not a
mechanism — materialise-once and identical-plan replay (below) are the two
known ways to discharge it, not its definition.

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

**5. The planner rule.** Compute-once is the semantic obligation, not
materialisation. LIR's nondeterminism is logical (which bag), never
physical: the executor is deterministic given a plan and the statement
snapshot. Therefore the obligation is discharged by either

- materialising the binding once and re-scoping per occurrence, or
- re-executing the *identical* sub-plan per occurrence.

What is forbidden: planning occurrences of a nondeterministic body
*differently* — distinct access paths may legitimately choose distinct
bags. Deterministic bodies keep full per-occurrence planning freedom
(path independence guarantees any two correct plans agree). If plan-replay
is relied on, "executor is deterministic given plan + snapshot" is
promoted from implementation property to stated invariant, with a
conformance test.

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

## Open questions (the implementation design session)

1. **Surface.** A document-level `bindings` map beside `nodes` with a
   `{kind: "ref", binding: name, scope: label}` node? A `let` node kind?
   Strawman for review, not decided:

   ```yaml
   bindings:
     expensive: { kind: aggregate, input: ..., scope: e, groups: ..., aggs: ... }
   nodes:
     a:   { kind: ref, binding: expensive, scope: a }
     b:   { kind: ref, binding: expensive, scope: b }
     j:   { kind: join, left: a, right: b, on: { eq a.user_id b.user_id } }
   root: { node: j, cardinality: many }
   ```

   Design against the adjacent family at the same time: `apply` (lateral),
   `recursive`/`recursive_ref`, `union` all want "reference a named
   relation" machinery (schema-flexibility.md).
2. **Scope re-exposure.** Does a ref expose the binding's output under the
   ref's scope label (mirroring `scan(orders, "a")` — keeps the base-table
   symmetry total, forces bindings to have a labelled/closed output)? Or
   re-expose interior scopes (breaks the symmetry, leaks the body)?
   Labelled output is the presumptive answer; confirm.
3. **Preflight extension.** Single-consumer everywhere except refs;
   binding bodies are trees; refs are the only fan-in. Reject: cycles
   through bindings, refs to unknown bindings, unused bindings
   (unreachable-definition rule extended), bindings shadowing node ids.
4. **Binder mechanics.** Where binding output slots live, how the
   occurrence renaming is represented in bound IR, how free slots of a
   correlated binding interact with the environment at each ref.
5. **Physical seam.** How the plan represents the committed choice — an
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
- **Preflight probes**: cyclic binding, dangling ref, unused binding,
  ordinary-node sharing (still rejected), binding/node id collision.
- **EXPLAIN**: one definition, N occurrence references, strategy visible.

## Decision record

2026-07-13 design discussion. Considered and rejected: fresh-occurrence
(macro) reference semantics — sound only on the deterministic fragment,
and LIR is deliberately not confined to it; a "self-join" of a
nondeterministic binding would not be a self-join, materialise-vs-recompute
would become observable (breaking path independence), and the conformance
oracle could not pin results. Also rejected: shared output slots across
occurrences (cannot express self-joins) and capture-at-reference (an
implicit lambda; parameterisation should be explicit if ever added).
