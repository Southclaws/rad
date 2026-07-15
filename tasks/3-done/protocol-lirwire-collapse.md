# protocol → lirwire/pirwire collapse

Status: **DONE** — committed as `7505bad`. The collapse to one generated wire
representation (no handwritten mirror, no bridge) is complete and the full suite
passes (engine incl. the reference-interpreter differential tests, protocol,
server, client, planner, e2e). The codegen output was deliberately carved off
into its own task — see [[codegen-generator-rewrite]] — since the generator
needs a structural rewrite, not just a type swap; until it lands, freshly
generated clients won't compile (its emit strings still name `protocol.*`, and
`examples/demo/generated/tracker.go` is removed).

## Test verification (all green except deferred codegen)

`go test ./rad/... ./tests/...` passes for: engine (all layers), `rad/protocol`
(round-trips + int64 precision), `rad/server/api`, `rad/codegen` (generator
unit test — string-level, does NOT compile its output), `tests/planner` (13
files), `tests/e2e`. `go build ./rad/... ./tests/...` clean; gofmt clean.

## Forest checks: relocated, not lost

Dropping `validateRelationForest` moved its checks to their natural owners
rather than losing them. Where the binder already saw the data, it holds the
check; the one property it structurally can't see (reachability) stays at
lowering:

- **Unused binding** → **binder** (`bind.go`). The binder resolves every ref in
  `bindRef`; it now records the target in a `used` set and, after binding root +
  bindings, rejects any local binding never referenced
  (`planner: binding %q is never referenced`). Program-statement bindings are
  exempt (an unconsumed statement result is legitimate). Sits alongside the
  existing binding validation (cycle detection, statement-shadow, duplicate
  output schema).
- **Unreachable/orphan node** → **`lowerQuery`** (`graphconv.go`). The binder
  cannot hold this: lowering discards orphans before it runs, so the binder's
  `lir.Query` has no node map. `lowerQuery` tracks a `reached` set as it walks
  from root + binding roots and rejects any `q.Nodes` entry never reached
  (`unreachable node definitions: [...]`, 422). This is inherently a wire-graph
  property.
- **Cycles, dangling refs, hidden scope, unknown binding, shared-consumer** were
  already covered by the binder (shared-consumer now surfaces as
  `duplicate scope`, since the shared node is lowered twice under one scope).

Tests restored to expect rejection: `TestStressUnreachableNodeRejected`,
`TestBindingUnusedRejected`, `TestBindingRootAlsoConsumedRejected` (rebuilt so
its query isolates the double-consume, not an incidental orphan). Full suite
green.

## Progress (this pass)

- **DONE, builds clean**: `protocol` (IR types + old `build.go`/`pir.go` +
  bridge all deleted; `lirjson.go`/`pirjson.go` are now thin
  marshal+validate over the generated unions), `lirwire`/`pirwire` builders
  (pointer union members; `LitOf`/`SetAny` literal stopgaps), `rad/server/api`
  (`graphconv.go` rewritten to lowering-only `lowerQuery(lirwire.Query)→lir`;
  wrapper + `validateWireShapes` + `validateRelationForest` dropped per the
  "rely on schema + binder" decision; `program.go` decodes each statement's
  raw relation → lirwire → lower), `rad/client` (data/program/client onto
  lirwire/pirwire), `tests/harness`, and the engine (never depended on the IR).
- **DONE**: test-suite conversion — `tests/planner/*` (13 files, incl. the
  flat-node walkers in `query_test.go` rewritten as union type-switches),
  `rad/server/api/*_test.go`, `rad/protocol/*_test.go`, `tests/harness`, one
  `tests/e2e` fixture. Shared test helpers in `rad/protocol/wire_helpers_test.go`
  and per-package `_test.go` files (`relBytes`, `mustValue`, `ptr*`).
- **DEFERRED to end** (only remaining work): codegen output —
  `codegen.go`/`runtime_go.go` still EMIT `protocol.*` builder strings, so a
  freshly generated client would not compile (the generator + its unit test
  pass because the test is string-level). `runtime_go.go`'s `scopeExpr` needs a
  union rebuild (inject scan scope into unscoped `col` refs) rather than flat
  field mutation. Then regen `examples/demo/generated/tracker.go` (user deleted
  it), fix `examples/demo/main.go`, build + `task demo`/`demo:ts`.
- Also still open (decoupled): `rad/ui/src/TableView.tsx` `/query`→`/execute` bug.

## graphconv cut (settled with user)

`graphQuery` wrapper was dead (test-only vestige of the removed `/query`
endpoint). The live PIR lowering (`graphQueryExternal`) is kept but stripped to
mechanical lowering only and renamed `lowerQuery`; per-kind + forest validation
deleted (schema + binder own it). `external`-binding backward-only enforcement
now rests on the binder.

## Decision (settled)

- Keep the `oneOf` schema + generated `lirwire`/`pirwire` union (strict wire
  validation; verbosity is a non-issue — we don't hand-write against the raw
  union, and codegen is being rethought anyway).
- Ergonomics via **colocated `build.go`** in `lirwire`/`pirwire` — validating
  constructors returning the generated structs; the shared construct/mutate
  surface for clients *and* the generative/metamorphic test suite. Regen
  rewrites only the generated file, never `build.go`.
- **Delete** the handwritten `protocol.Node`/`Expr`/`Query`/`Program`/`Statement`/
  `Field`/`GroupTerm`/`AggTerm`/`OrderTerm`/`RowsColumn`/`Root`/`Binding` + the
  `queryToWire`/`queryFromWire`/`nodeToWire`/…/`programToWire`/… bridge in
  `lirjson.go`/`pirjson.go` (~580 lines).
- `protocol` keeps only transport concerns: URL, `Problem`, `Record`,
  `TableInfo`/defs, the schema embeds + `Validate{LIR,PIR}JSON`, and thin
  `Marshal/Unmarshal{Query,Program}` that operate on `lirwire`/`pirwire` types
  directly (json.Marshal + validate; json.Unmarshal + validate — no conversion).
- `graphconv` becomes `lirwire.Query → engine lir.Query` (union type-switch
  instead of the flat-`kind` switch).

## Done

- `rad/protocol/lirwire/build.go` — node + expr constructors, `Query`/`Root`/…
  helpers, and `Value` `SetString/SetInt/SetFloat/SetBool/SetNull` (the raw-JSON
  hack; see Value note). Compiles.
- `rad/protocol/pirwire/build.go` — `Query`/`Create`/`Update`/`Delete` statement
  constructors + `Prog`. Compiles.

## Remaining (the consumer rewire — cascades, do together)

1. `protocol/lirjson.go` + `pirjson.go`: replace `Marshal/UnmarshalQuery` and
   `…Program` with the thin generated-type versions (drafted, reverted to keep
   the tree green); delete the bridge funcs. `pirvalidate.go` uses
   `statementNameAndRelation` — inline a small union-switch there.
2. `rad/server/api/graphconv.go`: rewrite `graphQueryExternal` (+ drop the
   test-only `graphQuery` wrapper) to walk `lirwire.Node`/`Expr` unions → `lir`.
3. `rad/server/api/program.go`: `programToEngine` takes `pirwire.Program`.
4. `rad/client/*.go`: `Execute(pirwire.Program)`, `QueryDatum(lirwire.Query)`,
   build via `pirwire`/`lirwire` builders.
5. `rad/codegen/codegen.go`: emit `lirwire`/`pirwire` builder calls (was
   `protocol.Node{…}`); regenerate `examples/demo/generated/tracker.go`. This is
   the intricate one — fold with / coordinate against the codegen rethink
   ([[generated-clients-rethink]]).
6. Tests: `protocol/{protocol_test,graph_test,pir_test}.go`,
   `server/api/graphconv_test.go`, `tests/e2e`, `tests/planner/planview_test.go`,
   `tests/harness/harness.go` — build via the new builders (several currently
   test the bridge itself and shrink to nothing).
7. Delete the `protocol` IR types once (1)–(6) reference nothing.

## Value note (deliberate stopgap)

`Value` is a raw-JSON `[]byte`; `build.go`'s `SetString/SetInt/SetFloat/SetBool`
format it by hand. This is temporary — the real rework (tagged union, or
force-to-string to protect Decimal/Float128/BigInt) lives in
[[typeless-value-encoding]]; the `SetX` helpers go away when it lands.

## Also queued, decoupled from this (do anytime)

- Dead-code sweep from the /query→PIR migration: delete unused `protocol`
  `QueryResponse`/`CreateRequest`/`UpdateRequest`/`DeleteRequest`/`RecordResponse`/
  `DeleteResponse`/`TxResponse`/`MigrateRequest` and `publicAPI.query`.
- **Bug**: `rad/ui/src/TableView.tsx` reads rows via the removed `POST /query`
  → 404; reroute through `/execute` (wrap the read as a one-statement program),
  like `QueryTool`.
