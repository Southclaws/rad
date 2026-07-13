# Query traces: a first-class observability artifact

Status: proposal — refine pre-build. One query produces one structured
trace covering its entire lifecycle: input, validation, binding, planning
decisions, every physical KV operation, timings, and result. Not log
output, and not a decorative admin screen — a **query observability
subsystem** whose artifact serves the admin UI, tests, demos, bug reports,
and external reviewers equally. Rad can do this better than most engines
because the query is structured from the moment it enters the server;
there is no parse-tree/plan/log impedance to bridge.

The demo argument is real: for database people (Glauber, Chris/SlateDB),
watching a structured LIR document bind into slots, choose an index range,
and issue exact decoded KV keys explains Rad faster than any prose.

## Grounding: what exists today, what doesn't

Design against the actual pipeline, not an imagined one:

- **Stages today**: schema validation → forest preflight → graphconv →
  bind → plan (single pass) → commit bindings → execute → datum → encode.
  There is **no rewrite/optimisation phase yet** — the planner is
  rule-based lowering. But real *decisions* worth tracing already exist:
  access-path selection (eq-prefix/range scoring, ordered pushdown),
  crossing extraction, correlation classification, the order tie-breaker,
  binding strategy (materialise/replay) and plan-choice sensitivity.
  "Pre/post-optimisation" today = bound plan + physical plan; the rewrite
  history section activates when rewrites exist (the RewriteTrace shape
  below is right and should be designed now, populated later).
- **Partial building blocks already built**:
  - `bound.Print` / `planner.PrintPlan` — the EXPLAIN precursors.
  - `countingStore`/`countingTxn` in the exec tests — KV-op counting as
    test infra; a trace layer is its generalisation (wrap `kv.KV` with an
    event recorder instead of counters).
  - `rad/server/keydecode.go` + the devtool UI's KV browser — raw→decoded
    key rendering partly exists.
  - The harness `Result` was explicitly built as the seam where "explain
    decoration" lands — this is that work.
  - Binding strategy and sensitivity already print in EXPLAIN.
- **Replay determinism** (stated invariant, tested): same query + same
  snapshot → identical execution, including the exact KV operation
  sequence. Consequences below — this is the property that makes Rad's
  tracing unusually strong.

## The artifact

One stable structured value, produced by the engine, consumed by
everything (UI, tests, CLI, JSON export, bug bundles):

```go
type QueryTrace struct {
    ID       TraceID
    Started  time.Time
    Duration time.Duration
    Status   TraceStatus // success | client_error | runtime_error | internal

    Input      InputTrace       // raw LIR, catalog revision, (future) params
    Validation ValidationTrace  // schema result, forest, binding dep graph
    Binding    BindingTrace     // scopes→slots, types, cardinality, ordering,
                                // free slots, sensitivity per binding
    Planning   PlanningTrace    // decisions (access path scores, extraction,
                                // strategies), plan snapshots, rewrite records
    Execution  ExecutionTrace   // operator spans, KV events, per-op metrics
    Result     ResultTrace      // cardinality, rows, encoded bytes, latency
}
```

Everything shares **stable IDs with lineage in both directions**:

```text
KV event → physical operator → bound node → LIR node id → catalog object
```

This is the same provenance thread the error-propagation proposal needs
(tasks/1-todo/error-propagation.md, sketch 3: node ids are erased before
the binder today). **Build provenance once, for both consumers** — an
error is a trace that stopped early; `trace.Status` + the failing span
should embed the same structured error the Problem JSON carries.

## Planning: decisions now, rewrites later

Record decisions as first-class entries even before an optimiser exists:

```text
Decision: access path
  node scan_t (tasks) → IndexRangeScan tasks_board_status_idx
  candidates: TableScan (score 0), tasks_board_status_idx (score 6: eq=1,
  range=yes, order=no)

Decision: crossing extraction
  filter_8 predicate contained exists(sub_3) → attach slot #22

Decision: binding strategy
  binding top_events → materialise (2 occurrences, plan-choice-sensitive)
```

When rewrites arrive, the ordered record model applies (initial snapshot →
rewrite records → final snapshot; never serialise the whole plan per
rewrite):

```go
type RewriteTrace struct {
    Rule        string
    Description string
    BeforeRoot  PlanNodeID
    AfterRoot   PlanNodeID
    Changed     []PlanNodeID
    Duration    time.Duration
}
```

Per logical node: identity, operator, row type, scopes, cardinality bounds,
known keys, ordering, nullability, free slots, sensitivity, binding
membership, consumer. Per physical node: kind, logical source, access
method, predicate, estimated rows (future), **actual rows, actual
duration**, children. Estimated-vs-actual with an error factor is the
single most useful line for database people once costing exists — design
the field pair now, leave estimates null until then.

## Execution: spans and KV events

Nested spans (query → validate/bind/plan → execute → per-operator →
per-KV-op), each with start, duration, parent, category, attributes,
status. Conceptually compatible with distributed tracing but Rad-schema'd
— a generic span cannot express plans, scopes, or binding semantics.

KV operations as structured events, not strings:

```go
type KVEvent struct {
    ID, ParentSpan     ...
    Time, Duration     ...
    Operation          KVOperation // get | scan | put | delete | txn_begin/commit/abort
    Key, RangeStart, RangeEnd []byte // gated by redaction level
    Decoded            KeyDecode    // table/index identity, pk meaning — keydecode.go
    KeysRead, BytesRead, BytesWritten uint64
    Result             string
}
```

Honesty at the abstraction boundary: these are **Rad KV operations**
against the `kv.KV` interface. SlateDB's internals (SST reads, object
store, cache hits) are a separate section that only appears if SlateDB
exposes them — never imply we traced beneath the boundary. (If Chris is
interested, a SlateDB stats hook feeding a `storage-engine` section is a
natural collaboration seam.)

Raw + decoded keys are the demo centrepiece — the relational→KV lowering
made visible — and mostly exist already via keydecode.

Per-operator metrics worth having from day one (they map to fields the
operators already know): rows in/out, KV requests/bytes, open/exec/close
timings. Join/aggregate/sort internals (matches, groups, comparisons) as
they're cheap to add. For bindings: strategy, occurrence count, rows
committed, materialisation bytes. For attaches: **outer rows, distinct
environments, deduplicated executions** — the numbers that demonstrate the
decorrelation machinery doing its job.

## Two ideas replay determinism unlocks (new here)

1. **Trace-by-re-execution.** Because execution is deterministic given
   plan + snapshot, tracing can default off with near-zero cost: an
   interesting query can be *re-run at trace level full* and — within the
   same statement snapshot (a held transaction, or a devtool-pinned
   snapshot) — yields the *identical* physical execution, event for event.
   Autocommit re-runs open a new snapshot, so post-hoc re-tracing is
   best-effort unless we pin; say so in the UI rather than pretending.
2. **Trace goldens and trace diffs.** Deterministic event sequences mean
   traces are diffable artifacts: conformance tests can assert against
   trace structure ("no full table scan", "binding planned once", "both
   refs used the committed frames", "prefix scan chosen") instead of
   string-golden PrintPlan output — less brittle, more precise. Two traces
   of the same query before/after an engine change diff cleanly; that is a
   regression-review tool for free.

Related unification: **EXPLAIN becomes a renderer over the trace's
planning section** — `PrintPlan` output derived from the same artifact the
UI consumes, one source of truth instead of a parallel printer.

## Wire and storage

- Request: `{"query": {...}, "trace": {"level": "full"}}` (or a header);
  response carries `trace_id`, full trace fetched separately —
  ordinary query responses never bloat. Levels:
  `off | summary | plan | operations | full` (prototype/devtool default:
  full; server default: off or summary).
- Storage: bounded in-memory ring buffer (last N traces, max bytes per
  trace, total cap, eviction). Admin UI lists recent / slowest / failed /
  most-KV-heavy. Do **not** persist traces into Rad itself yet — no
  recursive dependency on our own telemetry.
- **Redaction is a level, aligned with the error ADR's rule**: schema
  names (tables, indexes, columns, bindings, node ids) always fine;
  literal values, row samples, and key bytes only at `full` on the
  devtool, and even then behind an explicit toggle (`hide literals`,
  `truncate keys`, `hash values`). Traces leak user data by default if
  this isn't designed in.

## UI views (devtool, coordinated not monolithic)

Overview (status, phase timings, rows, KV ops, bytes, bindings) · LIR
forest (trees, refs, scopes, crossings) · plan view with runtime metrics
overlaid · decisions/rewrite timeline · KV event timeline (click →
highlights the issuing operator) · flame chart · later: result lineage
(output field → aggregate → column → KV value field — unusually achievable
given structured LIR, but explicitly a later stage).

## The conformance angle (what pays for it)

The same instrumentation serves tests, UI, demos, bug reports, and
optimisation work. Concretely, existing test machinery migrates onto it:
op-count assertions (countingStore) become KV-event assertions; plan-shape
goldens become trace-structure assertions; the harness `Result` gains the
trace and `ExpectPlan(...)`-style matchers. One subsystem, five consumers
— that is the cost justification.

Portable debug bundle as CLI:

```bash
rad query --trace full query.json > trace.json
rad trace view trace.json
```

## The demo sequence (acceptance shape for the prototype)

1. Submit structured LIR → 2. see the relation forest → 3. watch binding
into scopes/slots → 4. see a crossing extracted/decorrelated → 5. see the
access-path decision with scores → 6. inspect the exact decoded keys read
from SlateDB → 7. compare estimated vs actual rows (actuals now, estimates
when costing lands) → 8. flame chart → 9. binding strategy + dedup
counters → 10. (later) click a result field, trace it to storage.

## Open questions for refinement

- Package home for trace types: a leaf package (like `reject`) that exec
  emits into, server buffers, UI consumes — name and layer position.
- Emission mechanics: explicit trace parameter threaded through
  bind/plan/execute vs a context-carried recorder; cost when `off` must be
  ~zero (nil recorder, no allocations on the hot path).
- Do KV events wrap `kv.KV` (a tracing decorator, mirroring
  countingStore) or instrument call sites? Decorator is cleaner and gets
  transactions for free.
- Span/ID scheme shared with error `source` — settle jointly with
  tasks/1-todo/error-propagation.md (one provenance design, two
  consumers).
- Trace schema versioning: the artifact will be exported and attached to
  bug reports; it needs a `version` field from day one even while
  everything else stays intentionally unstable.
- How much of ValidationTrace is worth capturing on success (forest +
  dep graph are cheap; full diagnostics only on failure?).

Related: tasks/1-todo/error-propagation.md (shared provenance and status
model), the deferred EXPLAIN + per-op metrics item from
tasks/1-todo/next-steps.md (this subsumes it), rad/ui devtool (the consumer),
replay determinism (home/content/docs/engine/05-exec.mdx) as the enabling
invariant.
