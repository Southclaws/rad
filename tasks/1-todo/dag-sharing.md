# DAG sharing: an explicit binding construct for LIR

Status: semantics settled (2026-07-13 design discussion — see "The settled
framing" below); surface syntax, scope re-exposure, preflight extension, and
the physical seam remain open. This is the one battle-test finding (F7 in
tasks/3-done/lir-improvements.md) that is a grammar change; nothing gets
implemented until the remaining questions are settled too.

## The problem

LIR is a strict single-consumer tree: a node id names one inline definition,
consumed exactly once, with no sharing or materialisation identity. A query
that needs the same derived relation in two *sibling* contexts — a self-join
of a CTE-shaped derived table, or one derived relation used as both a nested
array and a fold — must duplicate the subtree under fresh ids and scopes.

(Scoped out after design discussion: "order by a computed value and also
return it" is NOT a motivating case. The labelled projection is the proper
relational expression of that — a column-level data dependency in one linear
pipeline — not a workaround. Sharing is only about sibling contexts.)

The pressures:

- **Query compilers.** A SQL frontend lowering a multi-referenced CTE or
  repeated subquery wants to emit one definition and reference it, not run
  its own duplication pass.
- **Physical reuse.** Duplicated subtrees are re-planned and re-executed;
  a binding construct is the natural seam for materialise-once.

## The settled framing: definition / value / occurrence

Design discussion (2026-07-13) settled the model. Base tables already
exhibit the full structure: one *definition* (the catalog entry), one
*value* per statement (the bag the snapshot pins), N *occurrences*
(`scan(orders, "a")`, `scan(orders, "b")` — fresh scopes and slots each).

Two points are agreed:

- **References instantiate.** Each occurrence of a binding gets fresh scope
  and slots — a self-join `join(ref expensive AS a, ref expensive AS b)`
  needs `a.user_id` and `b.user_id` as distinct bindings; literally shared
  output slots cannot express it. (This retracts the earlier "bound-once
  shared slots" framing in this file's first draft.)
- **Physical reuse is the planner's.** Materialise-once vs recompute must be
  invisible.

The third point is now also settled: **occurrences range over one bag.**
Fresh-occurrence (macro) semantics is semantics-preserving only on the
fully deterministic fragment, and LIR deliberately permits
declared-arbitrary relations (`slice` with no order denotes "some hundred
events," not a determined hundred). Expanding `let top = any-100(events)`
twice silently changes its meaning to two independent draws — so a binding
is the *choice point*: it commits to one member of the set of valid bags,
and every occurrence observes that choice. Hand-duplicated subtrees remain
two independent draws, which is now expressive rather than a footgun —
write it twice for two choices, reference it twice for one.

Consequences:

- A correlated binding commits one choice *per outer environment* — the
  semantics the attach machinery's per-DISTINCT-key deduplication already
  implements.
- **Planner rule**: compute-once is the semantic obligation, not
  materialisation. Replaying the identical physical plan against the one
  statement snapshot is deterministic (LIR's nondeterminism is logical —
  which bag — not physical), so materialise-once and same-plan-replay both
  discharge it. What is forbidden: planning occurrences of a
  nondeterministic body *differently* — different access paths may choose
  different bags. Deterministic bodies keep full per-occurrence planning
  freedom. Relying on plan-replay promotes "executor is deterministic given
  plan + snapshot" from implementation accident to stated invariant, with a
  conformance test.
- **Testable even though the bag isn't**: occurrence-consistency is exact —
  e.g. `let top = any-100(events)`, self-join on the full primary key, must
  return exactly 100 diagonal rows. That test goes in the battle corpus the
  day the construct lands.
- SQL precedent aligns: a query name denotes a table; Postgres materialises
  multi-referenced CTEs by default and makes divergence the opt-in.

## Why the current rules are load-bearing (do not weaken casually)

- **Single-consumer keeps the preflight cheap and total**: cycles, sharing,
  dangling refs, and unreachable definitions are all rejected by one walk,
  before binding.
- **Correlation is derived, not declared**: the binder computes free slots;
  a sub-relation is correlated because it references slots it doesn't
  produce. Sharing a definition across two *different* consumer
  environments breaks the current "one definition, one environment"
  assumption — a shared correlated node would need capture semantics
  (whose scopes? bound when?).
- **Node ids are labels, not variables**: they carry no identity, which is
  why the wire stays a flat map with no evaluation-order questions.

## The design questions a binding construct must answer

1. **Surface**: a `let`-style node (`{kind: "let", bindings: {...}, in: ...}`)?
   A document-level `bindings` section beside `nodes`? A `ref` node kind?
   The schema-flexibility review (tasks/1-todo/schema-flexibility.md)
   proposes related constructs worth designing against at the same time:
   `apply` (lateral), `recursive`/`recursive_ref`, `union` — all of which
   touch the same "reference a named relation" territory.
2. **Correlation and capture**: may a bound relation be correlated? If so,
   against which scopes — only those visible at the binding site (lexical),
   or the reference site? Lexical capture is almost certainly the sane
   answer; it must be stated and enforced.
3. **Scope visibility through references**: does a `ref` re-expose the bound
   relation's scopes, or does binding force a labelled output (aggregate/
   project-style closure)? Forcing a labelled output keeps the closure rules
   uniform.
4. **Evaluation semantics**: SETTLED (see above) — one committed choice per
   binding per environment; physical strategy free among provably-equal
   options (materialise, or replay the identical sub-plan).
5. **Preflight**: the single-consumer rule becomes "single-consumer except
   through refs to declared bindings"; cycle rejection must extend through
   binding references (no recursive bindings — recursion is its own future
   construct with its own rules).
6. **Binder mechanics**: bind the definition once; each reference mints
   fresh slots for the binding's *output row type* only, plus an
   occurrence→binding slot renaming. Linear in occurrences × output width —
   unlike macro expansion, which re-binds the whole subtree per occurrence
   and goes exponential under nested bindings.
7. **Physical seam**: where does the plan represent the binding —
   an explicit materialise operator, an attach-like spec, or planner
   freedom? What does EXPLAIN show?

## Constraints from settled doctrine

- Four primitives; a binding construct must justify itself as "could this be
  an ordinary relation?" — it can't, which is why it's a real grammar
  decision and not sugar.
- Path independence: physical choices (materialise vs re-evaluate) can
  never change results.
- The schema change lands in lir.schema.yaml (source of truth), regenerates
  lirwire, and extends the preflight — one coherent commit, tests first in
  the battle corpus style.

## Until then

The labelled-projection idiom is documented on the LIR docs page (Idioms)
and exercised by the corpus (TestStressTopNBySpendCrossing). It is the
recommended lowering for compilers too.
