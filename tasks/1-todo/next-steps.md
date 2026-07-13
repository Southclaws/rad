# Rad architecture review and directional roadmap

Status: active roadmap.

## Executive assessment

Rad is a strong and coherent proof of concept. LIR is the right foundation and should be evolved, not replaced. Its two-category model—relations and expressions, connected through explicit `Exists`, `First`, `Scalar`, and `Array` crossings—is substantially cleaner than an AST that accumulates special-purpose query constructs ([LIR contract](/Users/barney/Documents/rad/rad/engine/03_lir/doc.go:3)).

The primary risk is not missing SQL syntax. It is that several advertised invariants are not yet true across binding, planning, execution, and transport:

- Nested rows and arrays are not yet fully composable values.
- Correlated/lateral relations can bind but are not consistently executable.
- Nullability, deterministic ordering, cardinality, and error behavior are not proven planner properties.
- The planner is currently syntax-directed lowering with access-path selection, not yet a general optimizer.
- Query execution lacks a single catalog-and-data statement snapshot.
- The query contract is developing faster than its parameterization, result-shape, and compatibility model.

The focused LIR, planner, and executor suites pass. The protocol/client worktree is actively undergoing a tree-to-graph migration, so individual transport observations are snapshot-specific; the contract requirements below remain structural.

The exact synthesis is distinctive, but the product territory is not empty. Gel already offers a schema-generated, fully typed and composable query builder plus nested result shapes, while Prisma supports generated nested relation queries and alternate join/query execution strategies ([Gel query builder](https://docs.geldata.com/reference/using/js/querybuilder), [Gel shapes](https://docs.geldata.com/database/reference/edgeql/shapes), [Prisma relation queries](https://www.prisma.io/docs/orm/prisma-client/queries/relation-queries)). Rad’s defensible position should be:

> A database-native, language-independent typed relational IR whose planner and storage engine are co-designed for generated clients and application-shaped results.

That is a stronger and more precise claim than “a typed relational database without SQL has never existed.”

## What to preserve

- Keep the relation/expression separation and explicit cardinality crossings.
- Keep unbound names at the public boundary and dense bound slots internally ([bound relations](/Users/barney/Documents/rad/rad/engine/03_lir/bound/rel.go:8)).
- Preserve centralized three-valued logic and the rule that access-path narrowing does not replace the residual predicate ([expression evaluation](/Users/barney/Documents/rad/rad/engine/03_lir/bound/eval.go:25), [physical plan](/Users/barney/Documents/rad/rad/engine/04_planner/physical.go:3)).
- Continue testing a forcing query through both nested and batched execution paths; this is the correct architectural regression test.
- Keep the public LIR relatively small. Prefer frontend lowering and internal physical operators over exposing every SQL keyword directly.

## Foundational issues and decisions

### P0 — Make the logical model truthful

1. **Unify runtime values.** LIR says scalars, rows, and arrays are values, but execution stores nested results beside scalar slots in a separate channel ([runtime frame](/Users/barney/Documents/rad/rad/engine/05_exec/iter.go:17)). Consequently, a direct `Array(...)` projection may work while reprojecting that field or embedding `Exists(...)` inside another expression can fail or silently lose batching.

   Adopt one runtime datum capable of scalar, row, array, and null. Scalar operators must reject non-scalars. Extract every subquery crossing into an optimizer-visible Apply/Attach slot regardless of where it appears syntactically.

2. **Define dependent-relation semantics.** The binder permits the right side of a join to reference the left, while the current join executor constructs the right side independently ([join execution](/Users/barney/Documents/rad/rad/engine/05_exec/operators.go:515)).

   Treat correlation/parameterization as a first-class logical property. Lower dependent joins to an internal Apply or parameterized index-loop operator. Until supported, reject them explicitly rather than accepting a query with incorrect semantics. Join reordering must respect dependency edges.

3. **Derive types and cardinalities from visible outputs.**
   - A left join must expose every right-side field as nullable above the join.
   - Materialized rows must have unique field names; ambiguous raw join rows should require a projection.
   - Cardinality multiplication must use checked or saturating arithmetic, with overflow becoming unbounded.
   - Candidate keys and functional dependencies must survive projections and joins when provable.

4. **Make determinism explicit.** A syntactic `Order` is not necessarily a total order. `First` and stable pagination should require a proven total order, commonly by appending a surviving unique key. If arbitrary selection is desirable, expose it as a separate operation rather than letting `First` depend on physical input order.

5. **Define optimizer-safe expression effects.** Filter fusion and access narrowing can change whether a fallible expression such as division is evaluated. Ordinary operators and registered functions should therefore be pure, deterministic, and total by default. Fallible or volatile functions need explicit metadata and must form optimization barriers.

### P0 — Establish consistent database and public contracts

1. **Use one statement snapshot.** Binding currently reads catalog state separately from data execution, and autocommit execution can observe multiple storage moments ([query entrypoint](/Users/barney/Documents/rad/rad/engine/05_exec/execute.go:18)). Every query should bind and execute against one read-only catalog-and-data snapshot.

2. **Harden schema migration.** Generated application clients should not automatically own schema reconciliation. A stale application instance must never migrate a newer database backwards.

   Use catalog revision/fingerprint compare-and-swap, explicit migration authority, expand/contract workflows, destructive-change approval, and index states such as building/ready. Migration application must be atomic or resumable rather than a sequence of independently visible steps ([migration flow](/Users/barney/Documents/rad/rad/engine/06_frontend/migrate.go:16)).

3. **Finish the wire contract before compatibility becomes expensive.**
   - Keep LIR unversioned while it has no external consumers; add compatibility semantics only when a real boundary requires them.
   - Add schema revision/capabilities and typed parameters alongside the existing root and nodes.
   - Represent parameters separately from literals so plans can be fingerprinted and cached.
   - Return a general datum envelope supporting scalar, object, array, and null roots.
   - Validate tagged-union payload combinations exhaustively and impose node, depth, payload, and execution limits.
   - Canonicalize only after normalization, deterministic node renaming, and parameter extraction; raw caller node IDs and JSON ordering are not canonical.
   - Keep single-consumer tree semantics initially. Future graph sharing should use explicit `Let`/`Ref` semantics because shared correlated nodes have evaluation and capture implications.
   - Make cross-language integer representation lossless; JavaScript `number` cannot carry the full `int64` domain ([TypeScript generator](/Users/barney/Documents/rad/rad/codegen/typescript.go:8)).

4. **Separate logical and persistent representations.** LIR `Value` should not double as the permanent on-disk row contract. Introduce a versioned storage codec beneath the logical scalar/type/datum model so LIR evolution does not become a storage-format migration.

## SQL-like expressiveness

| Capability                                                                                               | Assessment and direction                                                                                                                                                     |
| -------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Select, filter, projection, computed scalars, inner/left joins, grouping, aggregation, ordering, slicing | Native and structurally sound once the semantic issues above are resolved.                                                                                                   |
| Nested and correlated results                                                                            | Central strength, but only after nested datums and dependent execution become first-class.                                                                                   |
| `HAVING`                                                                                                 | Already expressible as `Filter(Aggregate(...))`.                                                                                                                             |
| Scalar `DISTINCT`                                                                                        | Lowerable through a group-only aggregate; likely frontend sugar rather than a new logical node.                                                                              |
| Semi/anti joins                                                                                          | Lowerable through `Exists` and `Not(Exists)`, but should receive dedicated physical alternatives.                                                                            |
| Right join                                                                                               | Lowerable by swapping inputs and projecting the desired output order.                                                                                                        |
| Non-recursive CTE                                                                                        | Can be inlined. Shared evaluation should wait for explicit binding/sharing semantics.                                                                                        |
| `VALUES` / table-free query                                                                              | Missing; add a unit/values relation.                                                                                                                                         |
| `UNION ALL`                                                                                              | Clearest missing relational primitive and the first set operator to add.                                                                                                     |
| Union distinct, intersect, except, full outer join                                                       | Derive where practical after `UNION ALL`; add dedicated bag operators only when workloads justify them.                                                                      |
| Parameters, `CASE`, `COALESCE`, function calls                                                           | Missing expression capabilities needed before presenting LIR as broadly SQL-comparable.                                                                                      |
| `IN`, `ANY`, `ALL`                                                                                       | Existing conceptual lowerings are not null-correct under three-valued logic. Supply explicit semantics or differential-tested lowerings after conditional expressions exist. |
| Windows and recursive queries                                                                            | Genuine future logical operators; defer until a target workload requires them.                                                                                               |

Rad should describe present LIR as a composable SPJA-plus-aggregation IR with nested result shaping, not yet as SQL-equivalent.

## Directional roadmap

### 1. Freeze the semantic foundation

Before expanding LIR syntax, settle unified datums, crossing extraction, dependent joins, left-join nullability, total-order proofs, unique output names, cardinality overflow, expression effects, statement snapshots, wire versioning, parameters, and migration authority.

Exit criterion: syntactically harmless wrapping or reprojection cannot change correctness, batching visibility, or determinism.

### 2. Turn the planner into an optimizer

Evolve the current recursive lowering pass ([planner lowering](/Users/barney/Documents/rad/rad/engine/04_planner/plan_lir.go:31)) into:

`bind → normalize → derive logical properties → enumerate physical alternatives → cost/select → execute`

Track required slots, candidate keys, nullability, equality classes, exact cardinality bounds, estimated rows, partial/total order, parameterization, row goals, and blocking-memory requirements.

Introduce explicit alternatives for:

- Primary and secondary index ranges.
- Hash, index, nested-loop, and parameterized Apply joins.
- Semi/anti strategies.
- Bounded batch keyed lookup.
- Streaming scalar projection.
- Top-N, reverse scan, and covering-index execution.

Build EXPLAIN and per-operator metrics before investing in sophisticated statistics. Alternatives and observability create value immediately; histograms cannot improve a planner with only one physical choice.

### 3. Build a semantic oracle and bounded execution model

Implement a deliberately simple reference interpreter for bound logical LIR. Differentially compare every physical plan against it, including forced table/index access and nested/batched correlation.

Set explicit query budgets for memory, rows, storage operations, graph size, recursion depth, cancellation, and session lifetime. Replace unbounded buffering in projection, joins, sorting, aggregation, and root materialization with streaming or bounded/spillable operators according to workload priorities.

### 4. Grow capability through representative workloads

Drive new LIR features from several forcing workloads:

- Nested OLTP reads with multi-level correlation.
- Multi-join reporting with grouping and `HAVING`.
- Dynamic optional filters and stable pagination.
- Null-sensitive semi/anti and quantified predicates.
- Values and union composition.

Add `Values` and `UnionAll` first, followed by typed parameters, conditional expressions, null helpers, and a controlled pure-function registry. Defer windows and recursion until a forcing workload cannot be expressed cleanly without them.

### 5. Validate the product thesis independently of storage

Maintain a restricted SQLite or PostgreSQL compiler/backend for the same logical LIR. Use it as a differential oracle and strategic experiment, not as an immediate pivot.

If Rad’s generated-client experience is equally compelling on a mature relational engine, consider a gateway/engine product. If owning storage materially improves nested-query latency, deployment, or operational simplicity, demonstrate that advantage with measured workloads.

### 6. Treat production readiness as a separate gate

Before external deployment, require authentication and authorization, separation of data/migration/admin APIs, safe network defaults, typed and idempotent transaction errors, crash/reopen compatibility tests, storage-version migration, cancellation, quotas, and cross-language client conformance.

## Acceptance gates

- Any scalar, row, or array output can be referenced, reprojected, nested, and spread later.
- Wrapping a crossing in another expression does not turn batched execution into N+1 evaluation.
- Dependent joins are correctly planned or explicitly rejected.
- Left-join output nullability agrees with runtime null padding.
- `First` is either proven deterministic or explicitly arbitrary.
- Every physical alternative matches the logical interpreter for values, nulls, cardinality, and errors.
- Forced scan/index and nested/batched executions return identical results.
- Catalog binding and data reads share one statement snapshot.
- Canonical plan fingerprints are independent of caller node IDs and literal parameter values.
- Every root cardinality round-trips through Go and TypeScript clients without shape or integer loss.
- Query memory and storage-operation limits are enforced.
- Concurrent or stale clients cannot partially apply, reverse, or corrupt schema migrations.

## Assumptions

- The current LIR document and JSON Schema are normative; the older aggregation-shaped transport is transitional.
- Pre-release compatibility can be broken freely while the contract remains internal.
- SQL comparability means relational and null-semantic expressiveness, not reproducing SQL syntax.
- Functions are pure, deterministic, and total unless explicitly classified otherwise.
- The initial public graph remains single-consumer; general DAG sharing is deferred.
- SlateDB remains the primary implementation while the reference SQL backend tests whether owning storage is strategically justified.
