# Agent and contributor conventions

Conventions for anyone—human or agent—working in this repository. Keep this
short and enforceable; add a rule only when it has actually bitten.

Rad is a relational database and generated-client toolchain built on SlateDB.
The product is the developer experience:

```text
rad.schema.yaml → rad schema migrate → typed application code
```

The library and `rad` binary in `src/` are the canonical implementation. Write
code, comments, documentation, tasks, workflows, and release notes in the
present tense. Do not describe Rad as a rewrite, compare it to an earlier
implementation, or preserve construction history. Git owns history.

## Comments describe the code as it exists

Comments explain rationale, invariants, and non-obvious consequences. They do
not narrate the work that produced the code, cite temporary planning artifacts,
or bury deferred work.

Never delimit a source file with decorative banner comments. A section wanting
a banner is usually a concern wanting its own file or module. Never use Unicode
box-drawing bars in comments.

## Architecture

Rad is one Cargo package with a reusable library and a thin process binary.
The workspace also contains `tools/keyform`, the compiler for the storage
format specification.
Keep the numbered engine directory ladder and its downward dependency flow:

```text
src/engine/01_kv
src/engine/02_catalog
src/engine/03_lir
src/engine/04_planner
src/engine/05_exec
src/engine/06_frontend
```

- LIR is the storage-free relation graph and carries no transaction, session,
  transport, or physical-plan state.
- PIR is the atomic ordered program layer above LIR.
- `engine::frontend` is transport-neutral. HTTP lives outside the numbered
  engine under `src/http`.
- `src/process.rs` owns configuration, dependency construction, scheduler
  lifecycle, listener lifecycle, and orderly storage close.
- `protocol/lir.schema.yaml`, `protocol/pir.schema.yaml`,
  `protocol/storage.keyform`, the storage schemas under `protocol/storage/`,
  and `api/openapi.yaml` are normative. `protocol/storage.allocations` is the
  permanent allocation registry; keyform maintains it and rejects edits that
  change a released durable meaning. Never edit generated artifacts by hand
  (`src/engine/01_kv/manifest.rs`, `src/engine/01_kv/keyspace.rs`,
  `src/engine/01_kv/keys.rs`, `src/engine/05_exec/key_describe.rs`,
  `tests/storage_spec.rs`); regenerate with `task generate:storage`.
- Never assemble a durable storage key by hand. Build and parse keys through
  `engine::kv::keys` (or the `exec::codec` wrappers above it). A persisted
  encoding's meaning is immutable once released: new functionality adds new
  schemas, codecs, keyspaces, or physical generations, and never
  reinterprets existing bytes.

## Verification

Use the smallest focused test while iterating, then the proportional product
gate before handoff:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Focused confidence commands are documented in `Taskfile.yml`, including the
differential corpus, generated cases, deterministic scheduling, replay, and
real RustFS/Toxiproxy storage qualification.

The push/PR, scheduled confidence, overnight, and release workflows live in
`.github/workflows/ci.yml`, `confidence.yml`, `overnight.yml`, and
`release.yml`. Their manifests and retained traces are product evidence.
