# Structured text matching (`text_match`)

Add structured text matching to LIR so a SQL frontend can lower `LIKE` and
`ILIKE` patterns without embedding SQL pattern syntax into the IR. Matching
offers byte-exact literals and one deliberately narrow case-insensitive form:
Unicode simple case folding.

## Scope of this pass

**This pass: the LIR schema contract, engine machinery, reference executor,
and generative/oracle coverage.** The SQL frontend lowering (§3) lives on a
separate branch and is deferred — noted here so the shape it lowers into is
fixed first.

Non-goals, explicit: regex, alternation, character classes, linguistic
collations, normalization, accent folding, full Unicode folding, specialized
indexes, and the `_` single-character wildcard. Leave `TODO` comments wherever
the design is intentionally incomplete.

## 1. The `text_match` expression

`text_match` answers exactly one question: **does this text match this
structured pattern under this literal comparison rule?** `comparison` is not
a general collation. It has exactly two stable meanings:

- `exact` (the default): literal spans require identical UTF-8 bytes.
- `unicode_simple_fold`: corresponding Unicode code points compare under
  Unicode simple case folding, equivalent to Go's `strings.EqualFold`.

The latter is deterministic and locale-independent. It performs no Unicode
normalization, accent folding, transliteration, multi-code-point full folding,
or linguistic collation. A future cross-cutting collation design can govern
equality, ordering, grouping, uniqueness, matching, and indexes together
without changing what `unicode_simple_fold` means here.

`value` is matched against an anchored pattern built from an ordered list of
`parts`. The pattern is a **bind-time constant**: literal spans carry
strings, wildcards are structural, and no part is an expression. `value` is
the only operand that varies per row — so the matcher compiles once and
applies per row, and NULL can only enter through `value`.

> **TODO — the constant-pattern design deliberately excludes row-dependent
> patterns** such as `column LIKE other_column`: the pattern is not a
> per-row expression. A parameterized `LIKE` pattern is still fine — the
> frontend compiles it once from the *bound parameter value*, never as a
> per-row `Expr`. If dynamic patterns are genuinely needed later, add a
> separate explicit form (a new `TextMatchExprPart` kind, or a distinct
> expression) rather than making every literal part an arbitrary `Expr`.
> The `parts` `oneOf` makes that addition non-breaking.

Semantics:

- `value` must be text; the result is boolean under three-valued logic.
- The match is **fully anchored**: the concatenation of the parts must
  consume the entire `value`, not merely occur within it. `[literal "foo"]`
  matches only `"foo"`; contains is `[any_many, literal "foo", any_many]`;
  prefix is `[literal "foo", any_many]`; suffix is `[any_many, literal
  "foo"]`.
- If `value` is NULL the result is UNKNOWN; otherwise the match is a total
  TRUE / FALSE.
- `any_many` matches zero or more characters (SQL `%`), so an all-wildcard
  pattern `[any_many]` matches every non-NULL value including the empty
  string.
- Literal spans compare under `comparison`; omitting it means `exact`.

The exact matcher operates on bytes. The simple-fold matcher operates on code
points because Unicode simple folding maps one code point to one code point.
The "one character = byte / code point / grapheme" wildcard question still
does not arise until `any_one` (`_`) is added.

Schema (add `TextMatchExpr` to the `Expr` union; new `$defs`):

```yaml
TextMatchExpr:
  # kind: text_match
  # value: <text Expr>
  # parts: [TextMatchExprPart]   (minItems 1)
  # comparison: exact | unicode_simple_fold  (default exact)
  # TODO: this is deliberately the SQL LIKE wildcard set, structurally
  # represented — not a general pattern algebra. No alternation, repetition
  # of sub-patterns, character classes, or backreferences. A richer text
  # search facility is a separate expression, never a wider parts vocabulary.

TextMatchExprPart:
  oneOf:
    - LiteralTextMatchPart   # kind: literal, value: string (minLength 1)
    - AnyManyTextMatchPart   # kind: any_many  (SQL `%`)
    # TODO: AnyOneTextMatchPart (kind: any_one, SQL `_`) — deferred until the
    # "one character" question (byte vs code point vs grapheme) is decided.
    # The oneOf makes adding it non-breaking. Until then the SQL frontend
    # rejects an unescaped `_` with an unsupported-feature error.
```

Producers should emit a canonical parts list (coalesce adjacent literals,
collapse adjacent `any_many`) for plan stability, but the matcher must
accept any well-formed list. Literal spans are never empty; the empty
pattern (`LIKE ''`) is the frontend's job to lower to equality, not a
`text_match`.

## 2. Engine + reference execution

- **Binder**: check `value` is text, `parts` is well-formed and non-empty, and
  `comparison` is known; compile the constant pattern once. Exact literals
  remain strings; simple-fold literals compile to rune slices.
- **Evaluator**: the standard `%`-glob matcher — anchor the first and last
  literal spans, match interior spans in order across the `%` gaps. K3:
  NULL `value` → UNKNOWN, otherwise total. A bool-typed result flows through
  both `Eval` (a bool value) and `EvalPred` (a TriBool, NULL → UNKNOWN) with
  no predicate-path special case.
- **Reference executor**: refexec shares the one scalar evaluator (as it
  does for casts and branch selection), so there is no separate refexec
  matcher; the independent oracle lives at the test level — an exhaustive
  backtracking `%`-glob matcher cross-checked against the compiled
  `TextPattern` over every pattern/input pair up to a small bound.
- **Generative suite**: emit `text_match` patterns (literal + `any_many`
  mixes) with a `mustHit` floor, so the engine-vs-refexec differential
  exercises it end to end.

TODO comments to leave: eliminate the simple-fold path's per-row `[]rune`
allocation; `any_one`; folded indexes/planner support; and a separate
cross-cutting text-equivalence/collation design.

## 3. SQL frontend lowering — DEFERRED (SQL-frontend branch)

Not implemented on `main`. Recorded so the target shape is fixed:

- Tokenize a `LIKE`/`ILIKE` string into literal runs and wildcards → `parts`.
- Escaped `%` / `_` become ordinary characters inside a `literal` part;
  wildcards `%`/`_` never reach LIR.
- A `LIKE` pattern with no unescaped wildcard may lower to byte-exact equality;
  an `ILIKE` pattern still lowers to `text_match` because ordinary equality
  does not have simple-fold semantics.
- An unescaped `_` returns an unsupported-feature error until `any_one`
  lands.
- `LIKE` lowers to `text_match` with the default `exact` comparison. `ILIKE`
  lowers to `text_match` with `comparison: unicode_simple_fold`. This is an
  explicit compatibility approximation, not exact reproduction of
  locale-sensitive PostgreSQL behavior.

TODO: the eventual target is lowering into this structured representation,
never embedding SQL pattern syntax into LIR.

## 4. Planning — scan + filter

Every `text_match` executes as an ordinary scan + filter this pass.

TODO (deferred): recognize exact `[literal L, any_many]` as a prefix range
scan; folded indexes for `unicode_simple_fold`; reverse indexes for suffix;
trigram/TF-IDF indexes for broader search; residual predicate validation; and
composite-index prefix planning. Do not delay the feature for index support.

## 5. Tests

Engine-side (in scope) — binder + reference executor + differential:

- prefix `[lit, %]`, suffix `[%, lit]`, infix `[%, lit, %]`,
  equality-shaped `[lit]`, and multi-gap `[lit, %, lit, %, lit]` — match and
  miss for each;
- exact case-sensitivity (`Foo` does not match `foo`) and explicit simple-fold
  matching (`FOO` matches `foo`);
- no full-fold expansion (`Straße` does not match `STRASSE`), accent folding,
  normalization, or Turkish-locale special casing;
- NULL `value` → UNKNOWN;
- all-wildcard `[%]` matches every non-NULL value including empty;
- empty `value` against a non-empty pattern;
- UTF-8 literal spans, enough to document current byte-level behavior.

Frontend lowering tests are deferred with §3.

## 6. Stop when sufficient

Once the relevant Storyden tests pass (on the frontend branch, later): do not
broaden the implementation; leave the TODOs in place; record any unsupported
PostgreSQL patterns encountered; defer broader Unicode/collation semantics,
`any_one`, and indexing to their own design passes.
