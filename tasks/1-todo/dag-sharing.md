# DAG sharing: an explicit binding construct for LIR

Status: design-pending. This is the one battle-test finding (F7 in
tasks/3-done/lir-improvements.md) that is a grammar change, and the node-DAG
rules it touches are hard rules — nothing here gets implemented until the
design is settled explicitly.

## The problem

LIR is a strict single-consumer tree: a node id names one inline definition,
consumed exactly once, with no sharing or materialisation identity. Any
query that needs the same derived relation twice must either:

- duplicate the subtree under fresh ids and scopes, or
- restructure via the labelled-projection idiom (compute the value once in a
  `project` with a `scope`, then order/filter/re-project by that scope's
  columns above).

The idiom covers the common cases well — the battle corpus expressed
"order by a crossing value and also return it" this way without friction —
but two pressures will grow:

- **Query compilers.** A SQL frontend lowering a CTE, a repeated subquery,
  or `SELECT expr … ORDER BY expr` wants to emit one definition and
  reference it, not run its own duplication/restructuring pass.
- **Physical reuse.** Duplicated subtrees are re-planned and re-executed;
  a binding construct is the natural seam for materialise-once.

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
4. **Evaluation semantics**: is a binding evaluated at most once
   (memoised), exactly once (materialised), or is that deliberately
   unspecified (logical sharing only, physical choice free)? Unspecified-
   but-result-equivalent fits the path-independence doctrine; it must then
   be provable that re-evaluation and memoisation agree (pure relations do,
   but only against one statement snapshot — already guaranteed).
5. **Preflight**: the single-consumer rule becomes "single-consumer except
   through refs to declared bindings"; cycle rejection must extend through
   binding references (no recursive bindings — recursion is its own future
   construct with its own rules).
6. **Binder mechanics**: bound-once slot assignment means a shared relation
   has ONE set of output slots — two refs see the same slots. That is
   exactly what makes "order by it and project it" work, and exactly what a
   correlated-per-consumer semantics would break. This tension (shared
   slots vs per-consumer correlation) is the heart of the design.
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
