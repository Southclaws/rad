# Strip construction-narrative from all comments

Status: ready, do in one focused sweep. Enforce the CLAUDE.md comment rule
across the whole codebase: comments describe the code as it is now, never how /
when / in what order it was built. Doing this now is cheap; doing it at release
(when it's a blocker) is an ordeal that steals time from features.

## What to purge (all of it, everywhere)

Every comment that narrates the *making* of the code rather than the code:

1. **Work episodes** — "this arc", "this pass", "a later arc", "slice" /
   "later slices" (as work units, NOT the LIR `slice` node), "step 4/5",
   "session".
2. **Phantom versions** — "v1", "PIR v1", "LIR v1", "carried over from v1".
   It is v0; there is no prior version to reference.
3. **Planning artifacts** — "the oracle ADR", "the query-trace ADR", "the … ADR"
   (no ADRs exist), and named `tasks/` items (loose, deleted at release).
4. **Deferred work in comments** — "unify this later", "streaming is a later
   arc", "… for now", "later cleanup", "populate later". This is the important
   one: **extract genuine deferred work to a `tasks/` item first, THEN delete
   the comment.** A TODO in a comment is a TODO that never gets done.
5. **History** — "previously", "renamed from", "used to", "kept for", "this used
   to be". Git holds history and squashes it before release.

## Per-hit policy

For each offending comment:
- If it was explaining the code's *why* wrapped in construction-narrative →
  **rewrite** it to state the invariant/rationale timelessly (drop the arc/v1/
  ADR framing, keep the real insight).
- If it added nothing but narrative → **delete** it.
- If it names real future work → **move it to `tasks/`** (new or existing item),
  then delete the comment.

Do not weaken the codebase's genuine why-comments — those stay; only the
timeline/version/artifact/deferred/history residue goes.

## Scope

- All hand-written `*.go` across `rad/`, `cmd/`, `tests/`.
- Generator **templates** (`rad/codegen/*/templates/*.tmpl`) if they emit such
  comments — fix the template, then regenerate; never hand-edit generated
  output (`examples/demo/generated/*`), it is reproduced from the templates.
- Doc comments and the `.schema.yaml` description prose count too.

## Candidate hunt (noisy — review every hit in comment context)

Restrict to comment lines to cut false positives:

```
rg -n --type go '^\s*//.*\b(arc|ADR|v1|for now|later|step [0-9]|previously|renamed from|used to|kept for|this (pass|session)|carried over)\b'
```

Caveats: `slice` and `arc`/`later`/`used to` appear legitimately (the LIR
`slice` node, Go slices, ordinary prose), so this list is candidates, not
matches — judge each. Also sweep `.schema.yaml` and `.tmpl` separately.

## Canonical examples of what must go (the target, verbatim from the tree)

```
// … The structural invariant carried over from v1, now visible in the tree …
// … PIR v1 does not change row identity …
// (PrintPlan remains the planner's internal golden-test renderer for now;
//  unifying it onto PlanView is a later cleanup.)
// … designed in the query-trace ADR and populate later slices.
// … (step 4/5 of the oracle ADR).
// No numeric widening in comparisons this arc …
// forcingQuery is the arc's acceptance shape
// … Only literal bounds drive access paths this arc.
// streaming end-to-end is a later arc          ← becomes a tasks/ item
```

Each of these either rewrites to a timeless statement of the code's behaviour,
deletes outright, or (the deferred ones) becomes a `tasks/` entry.

Related: [[protocol-lirwire-collapse]] and the codegen tasks all landed comments
in this style — expect hits there. Rule lives in `CLAUDE.md`.
