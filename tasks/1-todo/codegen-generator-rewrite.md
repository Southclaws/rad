# Rewrite codegen: pluggable generators, templates, Ent/Prisma ergonomics

Status: **SKELETON DONE, demo green** — the pluggable architecture landed and
the loop is re-closed (freshly generated Go clients compile AND the Tracker
demo runs end to end over the regenerated client). Remaining: Ent/Prisma
ergonomics, TS templatization + Schemancer'd types, IR Go-ism purification, and
external `rad-gen-*` resolution. Split out of [[protocol-lirwire-collapse]]
(done, `7505bad`).

## Done (skeleton)

- `rad/codegen/generator.go` — `Generator` interface (`Generate(*Model, Options)
  ([]GeneratedFile, error)`) + a name→generator registry (`Register`/`Lookup`/
  `Languages`). Built-in generators self-register from `init()`; the CLI
  blank-imports them so the parent package never imports a generator (no cycle).
- `rad/codegen/ir.go` — the exported, language-agnostic-ish `Model`/`Table`/
  `Col`/`Rel` + `Build` + shared helpers (`GoName`/`GoType`/`Snake`/`UqCols`).
  Renamed the vibecode `SQLName` → `Name` (there is zero SQL in rad).
- `rad/codegen/golang/` — the Go generator, now **`text/template`** with
  embedded `templates/{client,runtime,table}.tmpl`, ported off `protocol.*`
  onto the `lirwire` builders (incl. the `scopeExpr` union rebuild, `Slice`
  offset/limit, `Desc *bool`/`Arg *Expr` pointer fields). Smoke test guards it.
- `rad/codegen/typescript/` — the TS generator moved into its package as a
  `Generator` impl (still `p()`-based; output byte-identical — the wire JSON it
  emits was never affected by the collapse).
- `cmd/rad/generate.go` — routes through the registry; writes `[]GeneratedFile`.
- Regenerated `examples/demo/generated/tracker.go`; `task demo` passes.

## Remaining (the flesh-out)

- Go = Ent ergonomics (per-table predicate packages → `lirwire.Expr`, fluent
  query builder, relation predicates), with the neutral `predicate` package as
  the cycle-breaker (verified against storyden/ent). Multi-package emission
  (`[]GeneratedFile` already supports it). Decide package layout then.
- TS = Prisma: templatize + Schemancer the TS types from the wire spec; build a
  real TS demo (defer per the plan).
- IR purification: drop the Go-isms (`Col.Field`, `Col.GoType`, `Table.Model`)
  so the Model is a clean language-agnostic contract; each generator computes
  its own names/types. Do this when designing the multi-package layout.
- External `rad-gen-*`: resolve an unknown `--lang` to an executable on `$PATH`,
  Model-as-JSON in / `[]GeneratedFile`-as-JSON out.
- Golden-file tests per generator (schemancer-style).

## Why now, not just a type swap

The `protocol.* → lirwire/pirwire` change is genuinely minimal and mechanical.
But the current generator is `p("...")` concatenation with no structure, and we
want the emitted clients to feel like **Ent (Go)** and **Prisma (TS)** — that
quality of output is not maintainable as concatenated strings. So restructure
first, then grow ergonomics.

Reference points (both authored/used by us):
- `../schemancer` — our own codegen: Schema → IR → `text/template` per-language
  `Generator`, a registry, golden-file tests, documented "add a generator" path.
  This task brings rad's codegen up to that bar. (schemancer uses an inline
  template const; we go one better with embedded `.tmpl` files.)
- `../storyden/internal/ent` — the Go ergonomic target: per-entity predicate
  packages (`account.NameEQ`, `NameContains`, `HasSessions`, `HasSessionsWith`,
  `OrderOption`) returning a first-class `predicate.Account`, composed by a
  fluent query builder (`.Where(...).WithSessions().Order(...)`).
- Prisma — the TS target: object-literal `where`/`include`/`orderBy`, deeply
  typed.

## Architecture (settled in discussion)

- **`Generator` interface + registry.** One interface,
  `Generate(in *IR, opts Options) ([]GeneratedFile, error)`, mirroring
  schemancer's `Generator`/`GeneratedFile`. A registry maps a name → generator.
  Built-in generators register in-process; the CLI resolves an unknown name to
  an external `rad-gen-<name>` executable on `$PATH` (git-extension style).
- **Built-in ⇄ external unified by a serializable IR.** Because
  `Generate(IR) → []GeneratedFile` is serializable, an external generator is the
  *same contract over a subprocess*: rad writes the IR as JSON to stdin, reads
  `[]GeneratedFile` as JSON from stdout. So **the IR is the real public
  contract** (versioned, language-agnostic: tables, columns, types, PKs,
  FKs/relations, indexes) — firm up today's `genModel`/`genTable`/`genCol`/
  `genRel` into that artifact, decoupled from rad-internal types. Third-party
  `rad-gen-*` tools are a later deliverable, but the contract is fixed now.
- **Per-language packages with embedded template folders.**
  `rad/codegen/{ir.go, generator.go (interface+registry)}`, then
  `rad/codegen/golang/` and `rad/codegen/typescript/`, each implementing
  `Generator` and owning `templates/*.tmpl` via `//go:embed`, sharing a FuncMap.
  Full `text/template` (ranges, conditionals, helpers) — real files, so they
  diff and highlight. Go + TS built in for now.
- **Golden-file tests** per generator (schemancer-style): schema → generate →
  compare to `expected.*`, and the Go output must compile.

## Emitted-API ergonomics (the substantive design; do after the skeleton)

- **Go = Ent.** Per-table predicate constructors compiling to `lirwire.Expr`:
  `tasks.StatusEQ("open")`, `tasks.PriorityGTE(3)`, `tasks.HasBoard()`,
  `tasks.HasBoardWith(boards.NameEQ(...))` (relation predicates → correlated LIR
  subqueries). Fluent query builder composing them + eager-loading relations:
  `client.Tasks.Query().Where(...).WithBoard().Order(tasks.ByPriority()).Limit(20).All(ctx)`.
- **TS = Prisma.** Object-literal `where`/`include`/`orderBy`, deeply typed,
  compiling to the same wire graph.
- Overlaps with [[generated-clients-rethink]] (first-class multi-statement
  program building, bulk input, the interactive-transaction question) — that
  task owns the *runtime program model*; this one owns the *generator + emitted
  builder surface*. Reconcile the ergonomic layer between them.

## Mechanical carry-over (must happen regardless of ergonomics)

- `runtime_go.go`'s `scopeExpr` mutates the flat `protocol.Expr` to inject a
  scan's scope into unscoped `col` refs; against the union it must **rebuild**
  the expr tree per variant. This is the one non-trivial port.
- `querySpec.filters` `[]*protocol.Expr` → `[]lirwire.Expr`; `assemble`/`chain`/
  `include` node construction → `lirwire` builders; the generated `View.Query`
  already takes `lirwire.Query`.
- Literal emission decision: `lirwire.LitOf(v)` (1:1, keeps the `any` path) vs
  typed `lirwire.SetInt/SetString/…` chosen per column at generation time
  (aligns with the [[typeless-value-encoding]] direction). Codegen knows the
  column type, so the typed path is available.
- Regenerate `examples/demo/generated/tracker.go`, fix `examples/demo/main.go`,
  run `task demo` / `task demo:ts`.

## Suggested sequence

1. Skeleton: `codegen` IR + `Generator` interface + registry; move Go into
   `codegen/golang` with embedded templates that emit **today's** API ported to
   `lirwire`/`pirwire`. Regen tracker, green the demo — re-closes the loop.
2. TS generator into `codegen/typescript` likewise; Go/TS parity + golden tests.
3. Grow ergonomics toward Ent/Prisma incrementally, each step guarded by golden
   tests (predicates → query builder → relation predicates → nested include).
4. (Later) `rad-gen-*` external-generator resolution in the CLI.

Related: [[generated-clients-rethink]], [[typeless-value-encoding]],
[[protocol-lirwire-collapse]] (done).
