# Ent-style Go client: predicate packages + fluent query builder

Status: design captured, not started. The dedicated write-up of the "Go feels
like Ent" ergonomics phase of [[codegen-generator-rewrite]] (whose skeleton is
done and demo-green). This is the _emitted API_ design; the generator machinery
(interface, registry, templates, IR) already exists to carry it.

Inspiration: **Ent** (`../storyden/internal/ent`). The goal is that a Rad Go
client reads like Ent — typed per-table predicates composed by a fluent builder,
relations traversed as predicates and eager-loads — all compiling down to
`lirwire.Query` on the wire. (TS = Prisma is the sibling goal, tracked in
[[codegen-generator-rewrite]]; do it when the real TS demo is built.)

## Target API surface (concrete)

Per-table predicate constructors, each returning a first-class typed predicate:

```go
tasks.StatusEQ("open")            // predicate.Tasks
tasks.PriorityGTE(3)
tasks.StatusIn("open", "doing")
tasks.AssigneeIDIsNull()
tasks.HasBoard()                  // relation existence
tasks.HasBoardWith(boards.NameEQ("Launch"))   // correlated relation predicate
```

Fluent query builder composing them + eager-loading relations + ordering:

```go
client.Tasks.Query().
    Where(tasks.StatusEQ("open"), tasks.PriorityGTE(3)).   // AND together
    WithBoard().                                           // eager-load parent
    WithComments(func(q *CommentsQuery){ q.Order(comments.ByCreatedAt()) }).
    Order(tasks.ByPriorityDesc()).
    Limit(20).
    All(ctx)
```

Order options as typed values: `tasks.ByPriority()`, `tasks.ByPriorityDesc()`.

## Package layout (~2 + N, exactly Ent's shape — verified against storyden/ent)

- **root package** (the generated client pkg): `Client`, `TasksQuery`, the
  `Tasks` model, mutations. All relations at the query-builder level live here,
  in ONE package (eager-loads like `WithBoard` are root methods), so no cycle is
  possible at that layer.
- **per-entity subpackage** (`tasks/`): field constants, predicate
  constructors, order options. Imports ONLY the neutral `predicate` package.
- **neutral `predicate/` package**: `type Tasks …`, `type Boards …` — the
  shared per-entity predicate types, importing NO entity package (a leaf).

## Cycle avoidance (the load-bearing detail — CONFIRMED viable)

Verified in storyden/ent: `predicate/predicate.go` is just
`type Account func(*sql.Selector)` importing only `dialect/sql` (zero entity
imports), and `account/where.go` imports `predicate` but NO sibling entity
package. So the entity-package import graph is a **star** (every `x/` → the leaf
`predicate/`), acyclic **by construction** — independent of how the _schema_
self-references or cycles (self-FK, A→B→A, multi-hop). The trick:
`tasks.HasBoardWith(preds ...predicate.Boards)` cross-references the neutral
`predicate.Boards` type, NOT the `boards` package; the boards table/FK facts it
needs are baked in as string literals at generation time. So `tasks/` never
imports `boards/`.

**Discipline the generator must hold:** entity packages NEVER import each other;
all cross-entity typing flows through `predicate`; all table/column/FK facts are
emitted as literals. Sketch the import graph on paper before templating — this
is where an Ent-style generator lives or dies.

## How predicates lower to LIR

- Scalar predicate (`tasks.StatusEQ`) → a `lirwire.Expr` builder scoped at bind
  time (the fluent builder injects the scan scope, like today's `scopeExpr`).
  `predicate.Tasks` is likely `func(scope string) lirwire.Expr` (or a small
  struct) so it can be scoped when assembled.
- Relation predicate (`tasks.HasBoardWith(...)`) → a correlated crossing: an
  `Exists`/`First` over a scan of the related table filtered by the FK-join
  equalities (the `Rel.Pairs` already computed in the IR) AND the inner
  predicate. This is exactly the correlated-subquery shape LIR is built for.
- Typed predicates give real safety: `predicate.Boards` can't be passed to a
  `Tasks` query — cheap once the package split exists.

## Why multi-package (framing that should drive the decision)

Primarily **ergonomics/namespacing**, not perf: `tasks.StatusEQ` only reads that
way because `tasks` is a package; one package forces `TasksStatusEQ`. The
compile/LSP-cache granularity (per-package recompile) is a real **bonus at
Storyden scale** (30+ tables, self-joins, many FKs), not the justification. If
we ever decided against the Ent feel, a single well-organized package is simpler
and fine for typical schemas — so the ergonomics are what justify the generator
complexity, and that's the call to make first.

## Prerequisites / interactions

- **IR purification** (from [[codegen-generator-rewrite]]): drop the Go-isms
  (`Col.Field`, `Col.GoType`, `Table.Model`) so the IR is language-agnostic and
  each generator computes its own names — do this alongside the layout design.
- `[]GeneratedFile` already supports a package tree (path-bearing `Path`), so
  the machinery needs no change; this is purely template + layout work.
- Overlaps [[generated-clients-rethink]] (first-class multi-statement programs,
  bulk input, interactive-tx question) — reconcile the ergonomic layer: that
  task owns the runtime program model, this owns the emitted query/predicate
  surface.

## Open decisions

- Exact `predicate.Tasks` representation (scoped-expr func vs struct wrapper).
- Field-op vocabulary per type (EQ/NEQ/GT/…/In/Contains/HasPrefix — how much of
  Ent's surface maps cleanly onto LIR's expression grammar; e.g. Contains/
  HasPrefix need string ops LIR may not have yet).
- Order-option representation and multi-term composition.
- Naming/pluralization robustness at 30+ tables (the current `modelName`
  pluralizer is naive — flagged for hardening).
- Golden-file tests per generator to lock output as it grows.

## Sequencing

1. IR purification + import-graph sketch (root/predicate/entity).
2. Predicate packages (fields → `lirwire.Expr`), typed `predicate.T`.
3. Fluent query builder composing predicates; eager-load relations.
4. Relation predicates (`HasWith`) → correlated crossings.
5. Golden tests; regen demo; harden naming.

Related: [[codegen-generator-rewrite]], [[generated-clients-rethink]],
[[protocol-lirwire-collapse]].
