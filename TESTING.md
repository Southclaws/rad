# Test architecture

This document identifies each Rust test surface, its root executable, its
support modules, and its replay contract. `Taskfile.yml` is the local command
index. The workflows under `.github/workflows` define CI campaign budgets and
artifact retention.

## Test surface map

Each file directly under `tests/` is a separate Cargo integration-test
executable. Unit tests under `src/` run in the `rad` library test executable.

| Surface | Root executable | Main support code | Primary assertion |
| --- | --- | --- | --- |
| Unit and component | [`src/lib.rs`](src/lib.rs) | Inline `#[cfg(test)]` modules | Local invariant or API contract |
| Authored differential corpus | [`tests/differential_corpus.rs`](tests/differential_corpus.rs) | [`tests/e2e`](tests/e2e), [`tests/oracle/exact.rs`](tests/oracle/exact.rs) | Authored result, production result, and reference result agree |
| Generated semantic differential | [`tests/generative_differential.rs`](tests/generative_differential.rs) | [`tests/generative`](tests/generative) | Independent models and all execution paths agree |
| Relation-cache differential | [`tests/generative_differential.rs`](tests/generative_differential.rs) | [`tests/generative/cache.rs`](tests/generative/cache.rs) | Cached, uncached, and reference outcomes agree |
| Schema DST | [`tests/schema_scheduler_simulation.rs`](tests/schema_scheduler_simulation.rs) | [`tests/support/schema_scheduler_scenario.rs`](tests/support/schema_scheduler_scenario.rs) | Every injected crash boundary converges to a valid state |
| Byte-identical DST replay | [`tests/schema_scheduler_determinism.rs`](tests/schema_scheduler_determinism.rs) | [`tests/support/schema_scheduler_scenario.rs`](tests/support/schema_scheduler_scenario.rs) | Two isolated runs produce identical semantic traces |
| Statistics DST | [`tests/statistics_simulation.rs`](tests/statistics_simulation.rs) | Statistics test doubles in the root file | One seed produces one stable model and relay trace |
| Reader snapshot DST | [`tests/reader_snapshot_simulation.rs`](tests/reader_snapshot_simulation.rs) | Test-local Turmoil hosts | Refresh and replacement schedules preserve snapshot rules |
| Relation-cache scheduling | [`tests/relation_cache_determinism.rs`](tests/relation_cache_determinism.rs) | Test-local event gate | Fill, wait, cancellation, and snapshot traces replay exactly |
| Storage crash DST | [`tests/storage_simulation.rs`](tests/storage_simulation.rs) | [`src/engine/01_kv/fault.rs`](src/engine/01_kv/fault.rs) | Uncommitted work is absent and acknowledged work is durable |
| Storage fault generation | [`tests/storage_fault_campaign.rs`](tests/storage_fault_campaign.rs) | [`src/engine/01_kv/fault.rs`](src/engine/01_kv/fault.rs) | Generated operation faults preserve transaction atomicity |
| Coverage-guided fuzzing | Binaries in [`fuzz/fuzz_targets`](fuzz/fuzz_targets) | [`fuzz/Cargo.toml`](fuzz/Cargo.toml), [`fuzz/corpus`](fuzz/corpus) | Parsers, codecs, snapshots, and cache paths preserve target invariants |
| Backend qualification | [`tests/kv_backend_qualification.rs`](tests/kv_backend_qualification.rs) | [`tests/support/s3.rs`](tests/support/s3.rs), [`tests/support/toxiproxy.rs`](tests/support/toxiproxy.rs) | Memory, file, and S3 implementations meet one KV contract |
| Process and replica tests | Root files such as [`tests/file_multi_replica.rs`](tests/file_multi_replica.rs) | [`tests/support/http_process.rs`](tests/support/http_process.rs), [`tests/support/multi_replica.rs`](tests/support/multi_replica.rs) | Separate processes preserve service and storage contracts |

`cargo test --test <name>` selects one integration-test executable. The name
is the root filename without `.rs`.

## Semantic oracles

The main semantic differential uses these execution paths:

- `Engine::execute` uses the selected physical plan and reusable caches.
- `Engine::execute_uncached` uses the selected physical plan without reusable
  caches.
- `Engine::execute_forced` converts narrowed access paths to table scans and
  keeps the residual predicate authoritative.
- `Engine::execute_nested` disables distinct-key batching for correlated
  evaluation.
- `Engine::execute_reference` binds LIR and runs `ReferenceExecutor` without a
  physical plan.

The entry points are in `src/engine/05_exec/engine.rs`. The logical
interpreter is in `src/engine/05_exec/reference.rs`. It shares scalar
evaluation and physical row decoding with production code. Focused tests cover
those shared components.

`tests/oracle/exact.rs` defines exact result comparison. It preserves array
order, object field order, scalar type, float bit patterns, error kind, error
reason, and complete error text. Tests must use this comparator when a cache or
execution path can change representation without changing semantics.

The generated suite also has implementations that do not use the engine
executor:

- `tests/generative/model.rs` computes result expectations for a bounded
  relational subset.
- `tests/generative/semantic_model.rs` computes small relation and set
  semantics.
- `tests/generative/program.rs` computes PIR state transitions, commit state,
  and rollback state.
- `tests/generative/nested_identity.rs` computes nested value identity cases.

These models reduce common-mode failures between the production executor and
the reference executor.

## Authored differential corpus

`tests/differential_corpus.rs` discovers the fixture directories under
`tests/e2e`. Each fixture defines a schema, seed rows, one PIR program, and
post-program assertions. The runner creates isolated production and reference
databases for each fixture.

The same root executable contains the relation-cache oracle for the authored
corpus. It compares uncached, cold-cache, and hot-cache outcomes. It also
requires successful cache hits and repeated failures. Failed results are not
admitted.

`tests/e2e/README.md` defines the fixture data contract. `task
test:differential` runs this executable.

## Generated semantic campaigns

`tests/generative_differential.rs` is the campaign root. The modules under
`tests/generative` separate generation from checking:

| Module | Responsibility |
| --- | --- |
| `mod.rs` | Decision tape, database construction, and cross-executor checks |
| `catalog.rs` | Valid catalog generation |
| `data.rs` | Rows that match the generated catalog |
| `query.rs` | Valid LIR relation and expression graphs |
| `recursive.rs` | Recursive relation shapes |
| `program.rs` | Valid and invalid PIR programs with a state model |
| `invalid.rs` | Near-valid LIR with a required structured rejection reason |
| `metamorphic.rs` | Equivalent query transformations |
| `model.rs` and `semantic_model.rs` | Independent bounded semantics |
| `cache.rs` | Root and physical relation-cache mutation scenarios |
| `coverage.rs` | Required generator family coverage |
| `shrink.rs` | Decision-tape reduction with failure preservation |
| `fixture.rs` | Replayable fixture emission |

Generation consumes a `Vec<u64>` decision tape through `Choices`. A seed
creates the tape with a fixed generator. A minimized tape is therefore the
portable replay identity. A shrink step deletes tape ranges or reduces one
value, regenerates a complete valid case, and accepts the edit only if the
same check still fails.

The root executable uses these environment controls:

| Control | Meaning |
| --- | --- |
| `RAD_GEN_*_CASES` | Case count for one generated family |
| `RAD_GEN_*_SEED` | First seed for one generated family |
| `RAD_GEN_REPLAY` | Comma-separated minimized decision tape |
| `RAD_GEN_REPLAY_KIND` | Generator family for the replay tape |
| `RAD_GEN_SHRINK_BUDGET` | Maximum shrink checks |
| `RAD_GEN_EMIT` | Directory for emitted regression fixtures |
| `RAD_GEN_SOAK_SECONDS` | Minimum wall-clock budget for a complete wave loop |
| `RAD_GEN_SOAK_SEED` | Root seed for a soak |
| `RAD_TEST_ARTIFACT_DIR` | Campaign context, result, and emitted fixture directory |

The soak executes each family in a child process. This isolates environment
controls and gives each failure one test identity. A time limit is checked
between complete waves. One started wave always runs every family.

`task test:generative` runs the normal matrix. The `generative_semantic_soak`
ignored test is the scheduled campaign root.

## Relation-cache confidence surfaces

`tests/generative/cache.rs` generates these physical shapes:

- One regular hash-build input.
- One grouped join-aggregate dimension.
- One stable build inside a recursive step.
- Two non-overlapping sibling hash builds.
- One hash build inside a nested relation.
- One hash build inside a derived binding.

Each case compares cached and uncached outcomes after relevant and unrelated
data changes. It also covers changed literals, binding names, root
cardinality, catalog definitions, storage generations, columns, indexes,
NULL keys, duplicate keys, empty probes, nested values, result order, and
large text values. Memory and local-file Slate drivers use the same case
model.

`tests/relation_cache_determinism.rs` stops execution at semantic cache events.
It controls fill publication and waiter wake-up without a timing assumption.
Each scenario runs twice and compares the complete `EngineEvent` sequence. It
covers old and current pinned snapshots, cancelled fill ownership, and a
cancelled woken waiter. A one-entry pressure scenario also replays physical
admission, oversized root rejection, and capacity eviction decisions.

The planner unit tests in `src/engine/04_planner/plan.rs` generate candidate
trees and compare the production antichain selection with exhaustive subset
enumeration. Cache unit tests in `src/engine/05_exec/relation_cache.rs` cover
identity, successful-result admission, failure exclusion, coalescing, and
exact entry and byte limits under concurrent fills.

`task test:relation-cache` runs the authored oracle, generated oracle, file
driver, and deterministic scheduling executable.

## Deterministic simulation

Schema DST uses Turmoil to control task order. The scenario implementation in
`tests/support/schema_scheduler_scenario.rs` owns:

- Scenario and crash-boundary enumeration.
- Seed derivation.
- Simulated hosts and storage.
- `EngineEvent` capture and crash injection.
- Convergence checks.
- Failure and campaign artifacts.

The semantic boundaries are defined in `src/engine/05_exec/events.rs`. They
identify staged storage work, checkpoint work, publication, commit start, and
commit success. A crash boundary matches an event type and operation type. A
test does not infer a boundary from elapsed time.

`tests/schema_scheduler_simulation.rs` has three entry points:

- `turmoil_restarts_schema_work_at_every_engine_boundary` is the bounded CI
  matrix.
- `turmoil_schema_work_soak` accepts `RAD_DST_SECONDS`, `RAD_DST_SEEDS`, and
  `RAD_DST_SEED_START`.
- `turmoil_schema_work_replay` accepts `RAD_DST_SEED`, `RAD_DST_SCENARIO`, and
  `RAD_DST_BOUNDARY`.

Failure artifacts use `RAD_TEST_ARTIFACT_DIR` or
`target/rad-test-artifacts/dst`. They contain the seed, scenario, boundary,
redacted semantic trace, error, and exact replay command.

`tests/schema_scheduler_determinism.rs` starts process-isolated trace helpers
twice for each boundary. It compares encoded JSON bytes and records a SHA-256
digest. It requires `RUSTFLAGS=--cfg tokio_unstable` and the
`dst-determinism` feature because Tokio task scheduling receives the seed.

`tests/statistics_simulation.rs` uses an injected `RuntimeEffects`, scripted
statistics sources, sinks, and relay outcomes. Its stronger ignored test also
uses seeded Tokio scheduling and compares the complete trace.

`tests/reader_snapshot_simulation.rs` and `tests/storage_simulation.rs` use
small Turmoil host models for snapshot replacement and process crash behavior.
`tests/storage_fault_campaign.rs` generates faults before and after KV
operations, then minimizes a failing fault rule list.

`task test:dst` runs all bounded pure-Rust simulation roots. `task
test:dst:determinism` runs the two same-seed trace comparisons. `task
test:dst:soak` and `task test:dst:replay` select the long schema campaign and
one schema replay.

## Coverage-guided fuzzing

`fuzz/Cargo.toml` defines one `cargo-fuzz` binary for each file in
`fuzz/fuzz_targets`. The pinned toolchain is `nightly-2026-07-20`.

| Target | Root source | Invariant |
| --- | --- | --- |
| `protocol_lir` | `fuzz/fuzz_targets/protocol_lir.rs` | Valid JSON LIR never violates lowering safety |
| `protocol_pir` | `fuzz/fuzz_targets/protocol_pir.rs` | Valid JSON PIR never violates lowering safety |
| `schema` | `fuzz/fuzz_targets/schema.rs` | Arbitrary schema bytes produce a value or a structured error |
| `ordered_tuple` | `fuzz/fuzz_targets/ordered_tuple.rs` | Decoded canonical tuples re-encode to the same bytes |
| `row_codec` | `fuzz/fuzz_targets/row_codec.rs` | Row decode, encode, field read, replacement, and removal agree |
| `reader_snapshot` | `fuzz/fuzz_targets/reader_snapshot.rs` | Generated writer and reader refresh schedules preserve snapshot results |
| `relation_cache` | `fuzz/fuzz_targets/relation_cache.rs` | Cached and uncached joins agree across dependency mutations |

Stable seed inputs live under `fuzz/corpus/<target>`. LibFuzzer can add local
coverage inputs to the same directory. Crash inputs default to
`fuzz/artifacts/<target>`. A crash input is the primary replay identity. Use
`cargo fuzz tmin` before moving a permanent case into a named corpus input or
an authored regression test.

`task test:fuzz:smoke` runs 256 inputs for every target. The scheduled fuzz
workflow runs each target in a separate matrix job. It derives a stable target
seed from the workflow seed, applies a wall-clock limit, and retains the run
context, log, and crash inputs.

## Storage and process qualification

The storage surfaces separate semantic simulation from real backend behavior:

- `tests/kv_backend_qualification.rs` defines the common transaction contract
  for memory, local file, and RustFS.
- `tests/storage_crash_recovery.rs` kills a real child process and checks
  reopen state.
- `tests/storage_network_faults.rs` uses Toxiproxy against RustFS.
- `tests/s3_latency.rs` applies a seeded latency schedule to RustFS requests.
- `tests/s3_multi_replica.rs` checks one writer and multiple readers.
- `tests/file_multi_replica.rs`, `tests/read_only_contract.rs`, and
  `tests/writer_fencing.rs` cover local multi-process rules.

Helpers in `tests/support` own process startup, port allocation, S3 container
configuration, Toxiproxy control, and shared workload fixtures. Tests that need
Docker are ignored in the normal Rust suite. The backend CI matrix and storage
campaigns run them explicitly.

## CI and artifacts

`.github/workflows/ci.yml` runs the required platform, backend, and
byte-identical determinism gates. `.github/workflows/confidence.yml` runs daily
schema DST, generated semantic, and storage campaigns.
`.github/workflows/overnight.yml` uses longer disjoint waves.
`.github/workflows/fuzz.yml` owns the scheduled coverage-guided target matrix.

Campaigns record the tested revision, source revision, seed, budget, and test
identity. Failure artifacts must not contain row values, keys, or other user
data unless the test data is generated and non-sensitive. Storage and DST
traces use redacted event forms for this reason.

The normal local gate is:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Use the smallest root executable during development. Run the normal gate after
the focused surface is green.
