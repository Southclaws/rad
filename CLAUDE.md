# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Rad is a relational database and generated-client toolchain in Go, built on SlateDB. The product is the developer experience, not the storage engine: `schema.rad → rad migrate → rad generate → typed application code, never write SQL`. It is an early proof of concept at v0.0.0 with zero users — never add backwards-compatibility shims or frame a change as "legacy" support; just make the change. See `README.md` for scope, `PRODUCT.md` for positioning, and `tasks/` (1-todo / 2-building / 3-done) for planned and in-flight work notes.

Also read `AGENTS.md`. Its key rule: never delimit a Go file into sections with banner comments — a section wanting a banner is a concern wanting its own file in the same package. Split the file instead, and remove banners you encounter in code you touch.

## The cgo gotcha (read first)

The SlateDB Go binding is cgo and links against a native library built into `lib/` from the pinned checkout in `third_party/slatedb`. Bare `go test` / `go build` fail to link without:

```
CGO_LDFLAGS="-L$PWD/lib -Wl,-rpath,$PWD/lib"   # from the repo root
```

The `task` wrappers set this for you — prefer them. On a fresh machine, `task slatedb:setup` clones and builds the native lib (version pinned by `go.mod`).

## Commands

Task (go-task) is the entry point; `task --list-all` shows everything.

- `task test` — all Go tests + compile the demo app
- `task test -- -run 'E2E/create_task'` — args after `--` pass through to `go test ./...`
- `CGO_LDFLAGS="-L$PWD/lib -Wl,-rpath,$PWD/lib" go test ./tests/e2e/ -run 'E2E/<fixture>'` — one e2e fixture directly
- `task test:planner -- -run X` — planner corpus only (tight loop)
- `task vet` — go vet all packages
- `task build` — mostly-static binaries into `bin/`
- `task demo` / `task up` — fresh server + Tracker demo (`up` keeps serving, admin UI on :7238)
- `task serve` — build the admin SPA then run the server
- `task generate` — regenerate the demo's typed clients (Go + TypeScript) from `examples/demo/schema.rad`
- `task protocol:generate` — regenerate LIR/PIR wire types and OpenAPI transport after editing `rad/protocol/*.schema.yaml` (needs the `schemancer` tool)
- `task ui:dev` — Vite dev server for the admin SPA (proxying to a running `task serve`)

The website in `home/` is a separate Next.js 16 + fumadocs project (pnpm, Node 24 via fnm, plain CSS, deployed on Vercel) — not part of the Go build.

## Architecture

Go workspace: root module `github.com/Southclaws/rad` plus `examples/demo` (a standalone consumer of the generated client).

### Request flow

Generated client (or `rad/client`) speaks `rad://host:7237` (plain HTTP; `rads://` for a TLS proxy — Rad never terminates TLS). Programs go to `POST /execute` as wire JSON → `rad/server/api` converts wire → engine types → planner binds and plans → exec runs against the KV store → nested JSON back. Errors are RFC 7807 problem+json with a coarse `code` and a fine-grained stable `reason`.

### Two IRs

- **LIR** (relation IR): the query language. A query is a graph of named relation nodes (`nodes`) plus a `root` with a cardinality (`many` / `first` / `exactly_one` / `scalar`). Storage-free, index-free, strategy-free.
- **PIR** (program IR): sits above LIR. A program is an atomic list of statements (`query` / `create` / `update` / `delete`), each wrapping an LIR relation; atomicity comes from multi-statement programs, not held transactions.

The wire grammars are generated: `rad/protocol/lir.schema.yaml` and `pir.schema.yaml` are the sources of truth, Schemancer emits the `lirwire`/`pirwire` union types, and there is no separate handwritten IR — the generated types are the one representation. Never edit `lirwire`/`pirwire` or the `.schema.json` files by hand.

### Engine layers (`rad/engine/`)

Numbered directories, dependencies only flow downward (see `rad/engine/doc.go`; package names drop the digits):

- `01_kv` — ordered KV + transactions, key encoding, SlateDB adapter
- `02_catalog` — schema: tables, columns, indexes, constraints; catalog _mode_ (`direct` or `schema`) is set once at database init and immutable after
- `03_lir` — the relation-graph IR: values, types, three-valued logic, unbound/bound relations
- `04_planner` — bound LIR → physical plan: binding, analyses, access paths, correlation classification
- `05_exec` — physical plan → KV operations; includes `refexec`, a reference interpreter used as a testing oracle
- `06_frontend` — public engine interface the server calls into

### Other packages

- `rad/protocol` — transport-neutral wire vocabulary: `rad://` URIs, LIR/PIR JSON validation and marshalling. No engine imports.
- `rad/api` — ogen-generated OpenAPI transport types (`openapi.yaml`).
- `rad/server` — HTTP server: wire API on :7237, admin/devtool UI on :7238. `rad/server/ui` embeds the SPA built from `rad/ui` (React + Vite) via go:embed.
- `rad/client` — handwritten Go client (catalog, data, program execution). The generated clients are the intended application-facing API.
- `rad/codegen` — client generators behind the `Generator` interface (language-agnostic `Model` in, `[]GeneratedFile` out); generators self-register from `init()` and the CLI blank-imports them.
- `cmd/rad` — CLI: `serve`, `migrate`, `generate`, `validate`.

Server config comes from env, flags override: `RAD_ADDR` (default :7237), `RAD_STORAGE` (`memory` | `file` | `s3`), `RAD_DATA_DIR`, `RAD_CATALOG_MODE`.

## Tests

- `tests/e2e/` — fixture-driven conformance suite: each directory is one scenario (`schema.rad` + `seed.json` + `test_<name>.json`) run through the real client → server → plan → execute path. The suite grows by adding directories, never by editing the runner. Read `tests/e2e/README.md` before writing a fixture — determinism rules (no `uuid()`/`now_ms()` defaults on asserted columns) and mutation-result ordering caveats matter.
- `tests/planner/` — planner battle-test corpus.
- `tests/harness/` — shared in-process client/server harness.
- `rad/engine/05_exec/refexec` — reference interpreter; engine results are checked against it as an oracle in engine tests.
