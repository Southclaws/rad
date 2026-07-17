# Query optimisation: semantic rewrites over bound LIR

Status: idea, not scheduled.

The current planner is intentionally correctness-first. Given a valid bound LIR
program, it chooses executable operators but performs little or no semantic
transformation.

This document proposes a future optimisation phase.

Its purpose is **not** to invent a different query. Its purpose is to produce
an equivalent bound LIR program that is cheaper to execute while preserving
every observable behaviour already defined by the language.

Correctness comes first. Every optimisation is optional.

## Position in the pipeline

Today:

```text
Wire
    ↓
Unbound LIR
    ↓
Binder
    ↓
Bound LIR
    ↓
Planner
    ↓
Physical plan
    ↓
Executor
```

Future:

```text
Wire
    ↓
Unbound LIR
    ↓
Binder
    ↓
Bound LIR
    ↓
Optimiser
    ↓
Optimised Bound LIR
    ↓
Planner
    ↓
Physical plan
    ↓
Executor
```

The optimiser consumes and produces **bound LIR**.

It does not operate on the wire format and it does not rewrite physical plans.

This separation keeps every optimisation independently testable and allows the
planner to remain a mechanical lowering from logical operators to physical
operators.

## Philosophy

The optimiser is a sequence of semantics-preserving rewrites.

Every transformation must preserve:

- row membership;
- multiplicity;
- row type;
- observable ordering;
- NULL semantics;
- correlation;
- binding semantics;
- recursive semantics.

If a transformation cannot be proven equivalent, it must not be applied.

An optimisation may always be disabled without changing query results.

## Rewrite-based design

Optimisation consists of many small local transformations rather than one large
planner.

Examples:

```text
Filter(Filter(x))
⇒ Filter(x)
```

```text
Distinct(Distinct(x))
⇒ Distinct(x)
```

```text
Slice(offset=0)
⇒ identity
```

```text
Project(Project(x))
⇒ merged project
```

Each rule should be understandable in isolation.

## Categories

### Structural simplification

Remove redundant operators.

Examples:

```text
Distinct(Distinct(x))
⇒ Distinct(x)
```

```text
Order(Order(x))
⇒ outer order
```

```text
Slice(offset=0, limit=nil)
⇒ identity
```

```text
Filter(TRUE)
⇒ identity
```

```text
Project(spread only)
⇒ identity
```

where semantics permit.

## Predicate optimisation

Simplify expressions.

Examples:

```text
TRUE AND x
⇒ x
```

```text
FALSE OR x
⇒ x
```

```text
NOT(NOT(x))
⇒ x
```

```text
1 + 2
⇒ 3
```

Constant folding belongs here.

Expression simplification must preserve SQL three-valued logic.

## Pushdown

Move operators closer to data.

Typical examples:

```text
Filter
```

below

```text
Project
```

when safe.

Or

```text
Filter
```

below

```text
Join
```

when it references only one side.

Pushdown reduces intermediate cardinality.

## Projection pruning

Remove unused columns.

If later operators never reference a column, avoid carrying it through the
pipeline.

This reduces memory usage and executor work.

## Join optimisation

Potential future rewrites include:

- predicate pushdown;
- join reordering;
- outer-to-inner conversion where provable;
- redundant join removal;
- join elimination via foreign-key knowledge.

These require progressively richer metadata and should be introduced
incrementally.

## Distinct and set operations

Future set operators naturally introduce algebraic identities.

Examples:

```text
Distinct(Distinct(x))
⇒ Distinct(x)
```

```text
Distinct(Union(all, a, b))
⇒ Union(distinct, a, b)
```

```text
Union(distinct, Distinct(a), Distinct(b))
⇒ Union(distinct, a, b)
```

Equivalent identities exist for intersection and difference.

## Ordering

Ordering should be removed whenever it cannot affect any observable result.

Examples:

```text
Order
    ↓
Aggregate
```

may not require ordering.

Conversely:

```text
Order
    ↓
Slice
```

must never be reordered.

The optimiser must preserve the determinism guarantees already defined by LIR.

## Cardinality reasoning

The binder already establishes substantial semantic information.

Future optimisation should propagate facts such as:

- at most one row;
- complete-row uniqueness;
- empty relation;
- global aggregate;
- known keys.

These facts enable:

- redundant operators;
- stronger planner choices;
- determinism proofs;
- join elimination.

## Recursive bindings

Recursive bindings remain logical operators.

Optimisation may simplify the anchor or step independently but must not change:

- accumulation mode;
- recursive frontier semantics;
- recursive well-formedness.

The optimiser should treat recursive bindings as optimisation boundaries until
stronger proofs are available.

## Rule organisation

Rules should be independent.

A rule should consist of:

- pattern;
- preconditions;
- replacement;
- proof sketch.

Example:

```text
Pattern

Distinct(Distinct(x))

Preconditions

None.

Replacement

Distinct(x)

Reason

Distinct is idempotent.
```

Keeping rules independent makes them individually testable.

## Statistics

The initial optimiser should require no statistics.

Statistics become valuable later for:

- join ordering;
- access path selection;
- cost estimation.

Semantic rewrites should remain valid regardless of statistics.

## Cost-based planning

A future cost model may choose among several equivalent plans.

Examples:

- hash join vs merge join;
- hash distinct vs sort distinct;
- index scan vs full scan.

This should remain separate from semantic optimisation.

The optimiser generates equivalent logical alternatives.

The planner chooses one physical implementation.

## Testing philosophy

Every optimisation rule should have:

- direct unit tests;
- planner tests;
- differential tests.

The differential oracle should execute:

```text
original bound LIR
```

and

```text
optimised bound LIR
```

and assert identical observable results.

Optimisation bugs should therefore appear as semantic mismatches rather than
executor failures.

## Long-term direction

Initially the optimiser may simply be:

```text
repeat

    apply local rewrite

until fixed point
```

As the rule set grows this may evolve toward a memo- or rule-based optimiser
(Cascades-style), but correctness should always take priority over search
space.

The optimiser should remain a library of individually understandable rewrite
rules rather than one monolithic planning algorithm.

## Initial milestone

The first optimisation milestone should focus only on deterministic,
obviously-correct rewrites.

Candidate rules:

- remove identity slice;
- remove identity filter;
- remove identity project;
- merge nested projects;
- merge nested filters;
- eliminate double distinct;
- constant folding;
- boolean simplification;
- predicate pushdown;
- projection pruning.

None of these require statistics.

Together they establish the optimisation framework before introducing
cost-based decisions or join reordering.

## Decision record

- Optimisation operates on bound LIR, not wire LIR or physical plans.
- The planner remains a logical-to-physical lowering phase.
- Optimisation is a collection of independent semantic rewrite rules.
- Statistics and cost models are future enhancements rather than prerequisites.
- Every optimisation must be individually testable and provably semantics
  preserving.
