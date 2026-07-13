# Binding validation gaps

Small hardening items found by auditing the new schema shape (bindings +
ref) across all four validation layers: JSON Schema, the kind-directed
best-match pass (lirvalidate.go), the forest preflight (graphconv.go), and
the binder. The load-bearing invariants are all covered and tested —
shared nodes across trees, binding roots that are also consumed, refs to
unknown bindings, unused bindings, binding cycles (including through
crossings and self-referencing roots), unreachable nodes, hidden interior
scopes, duplicate output columns. What follows is the residue.

## Gaps to close

1. **`bindings: {}` is schema-valid.** `nodes` requires `minProperties: 1`;
   `bindings` has no such floor, so an empty-but-present bindings object
   passes. Strictness parity says reject it.
   Fix: `minProperties: 1` on `Query.properties.bindings` in
   lir.schema.yaml + regen; a named error in the validation wrapper
   ("bindings must not be empty when present") so the rejection isn't a raw
   minProperties dump. Wire probe test.

2. **Binding-level schema failures bypass the best-match pass.** The F1
   machinery (lirvalidate.go) drills into `nodes` only. A malformed binding
   value — `{"node": ""}`, a wrong field, an empty binding name — falls
   back to the raw validator error instead of `binding "x": node: length
   must be >= 1`. The `Binding` def is a plain object (no oneOf), so the
   raw error is survivable, but it should meet the same bar as node errors.
   Fix: extend `validationDetail` to walk `doc["bindings"]` (sorted) and
   re-validate failing entries against the resolved `#/$defs/Binding`,
   reporting `binding %q: <specific rule>`. Wire probe tests for the empty
   node string and a cross-field payload.

3. **Empty binding name at the engine boundary.** HTTP is covered
   (`propertyNames: {minLength: 1}`), but engine-direct callers construct
   `lir.Query{Bindings: map[string]lir.Relation{"": …}}` unchecked; the
   failure mode today is a confusing downstream error. One line in
   `bindingOrder` (or the Bind loop): reject the empty name with a
   `planner:` input error. Binder test.

4. **Untested-but-legal shapes worth pinning.**
   - *Alias bindings*: a binding whose root node is itself a `ref`
     (`bindings: {a: {node: r}}, r = {kind: ref, binding: b, …}`) is legal
     and works (forest walk, dependency order, and occurrence slots all
     handle it) — but no test says so. Add a positive corpus test so the
     capability doesn't regress silently.
   - *Duplicate ref scopes*: two refs sharing a scope label are rejected by
     the binder's query-wide label rule; add the explicit probe
     (`duplicate scope "a"`) so the ref path is pinned, not just scans.

## Noted, deliberately not part of this task

- **Node-count / depth / payload / execution limits** are still absent
  everywhere (the 4 MB body cap is the only bound) — a pre-existing item
  from the original architecture review, not a bindings regression:
  bindings preserve wire-size ≈ plan-size (the anti-exponential tests pin
  it), so they don't change the calculus. Belongs to the wider wire-
  hardening task when it happens.
- **Binding-name / node-id collisions stay legal** by ADR decision (three
  separate namespaces; rejection would be a readability diagnostic, not an
  invariant). Revisit only if confusion shows up in practice.

## Shape of the work

Schema floor + regen (1), lirvalidate extension (2), binder line (3), four
small tests (1–4). No grammar changes beyond the minProperties floor; no
engine semantics touched. Roughly one short commit.
