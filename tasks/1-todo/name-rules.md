# Name rules for tables, columns, and indexes

Status: todo — deliberately deferred from the direct-catalog-mode phase
(2026-07-13): the catalog currently accepts any non-empty name, and the
RESTful catalog API puts names in URL paths, relying on URL-encoding to
carry arbitrary UTF-8. Decide the actual rules once, in one place.

## Why it needs deciding

Names now appear in more surfaces than before, each with its own
tolerance:

- **URL paths** (`PATCH /tables/{table}/columns/{column}`) — anything
  URL-encodes, but names with `/`, `.`, whitespace, or emptiness make for
  hostile URLs and confusing logs.
- **schema.rad** — YAML keys; currently whatever the parser accepts.
- **Generated code** (Go + TS clients) — names become identifiers;
  codegen must mangle or reject what it can't express (`user-events`,
  `2fa_codes`, names differing only by case or by mangle-collision).
- **Catalog keyspace** — `/rad/catalog/table_name/<name>` is a plain
  string key; names containing the key prefix's separator are fine today
  (no parsing back) but worth confirming stays true.
- **LIR scopes and output columns** — separate namespaces, already
  validated for emptiness at the binder; alignment worth checking.
- **The devtool UI** — display-only, tolerant.

## Shape of the decision (when picked up)

- One validation function in 02_catalog, applied at every creation and
  rename site (CreateTable, CreateColumn, RenameTable/Column,
  CreateIndex) — the single mutation surface means one enforcement point
  covers the reconciler and the imperative API alike.
- Candidate rule to argue from: non-empty, no leading/trailing
  whitespace, no control characters; probably NFC-normalised UTF-8;
  possibly a length cap. Whether to restrict to identifier-ish names
  (letters/digits/underscore) is the real debate — codegen wants it,
  playground friendliness doesn't. Codegen mangling with collision
  detection may reconcile the two.
- Case sensitivity: names are currently byte-compared everywhere;
  document that as the rule or normalise — but pick explicitly.
- Reserved names: anything starting with `rad_`/`_rad`? Decide before
  system tables/virtual relations exist, not after.

Non-goal: SQL-compat identifier rules. Rad names are API strings, not
tokens in a language.

Related: tasks/3-done/direct-catalog-mode.md (where the deferral was
made), rad/codegen (identifier mangling today), 02_catalog validation.
