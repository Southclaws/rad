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
- `protocol/lir.schema.yaml`, `protocol/pir.schema.yaml`, and
  `api/openapi.yaml` are normative. Generated wire and HTTP types are never
  edited by hand.

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
