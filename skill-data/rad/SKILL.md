---
name: rad
description: Use Rad's schema-first relational database and typed-client toolchain. Load when initializing or configuring a Rad project, editing or validating rad.schema.yaml, planning or applying schema changes, recovering rad.state, generating clients, diagnosing project or server health, monitoring online schema transitions, or operating the rad CLI.
---

# Rad

Treat `rad.schema.yaml` as desired database state. Use Rad to validate it, plan the semantic difference from the live catalog, apply it transactionally, record the accepted state, and generate an exact-schema client.

## Discover the installed surface

Run these before guessing flags or behavior:

```sh
rad --help
rad <command> --help
rad spec --format json
```

Prefer `--output json` for finite commands. Add `--non-interactive` in unattended work so Rad fails instead of prompting.

## Preserve the project invariants

- Edit `rad.schema.yaml`; never edit files under `rad.state/`.
- Preserve table and column IDs when renaming. Changing an ID means deletion plus creation and can lose data.
- Treat `rad.state/schema.lock.json` as the pointer to the immutable accepted snapshot used for generation.
- Do not generate from unapplied desired changes. `rad generate` refuses this state intentionally.
- Never add `--accept-data-loss` merely to make a command pass. Review the JSON diff and obtain explicit user consent first.
- Treat `rad schema pull` as recovery from a correct server, not as the normal migration workflow.

Read [references/schema.md](references/schema.md) before authoring non-trivial tables, indexes, foreign keys, defaults, conversions, or destructive changes.

## Initialize a project

Use guided setup for a person at a terminal:

```sh
rad init
```

Use explicit unattended setup for automation:

```sh
rad --non-interactive --output json init --yes ./app
```

Initialization creates `rad.config.yaml` and `rad.schema.yaml` without overwriting either file. It does not create accepted state.

## Change a schema safely

Follow this loop:

```sh
rad --output json validate
rad --output json schema diff
rad --non-interactive --output json schema migrate
rad --output json schema status
```

Interpret the diff before migration:

1. Stop and fix every `blocking` finding. No flag bypasses one.
2. If `destructive` is non-empty, summarize the exact loss and ask the user for consent.
3. Only after consent, rerun migration with `--accept-data-loss`.
4. Treat a catalog conflict as a stale plan: run diff again rather than retrying the old conclusion.
5. Verify the final status is `synchronized`.

Migration replans transactionally, waits for accepted online work, writes the accepted snapshot and lockfile, then regenerates configured clients. A generation failure after commit does not roll back the accepted database schema; fix generation and run `rad generate`.

## Recover local state

When the server is the intended source of truth and local accepted state is missing or behind:

```sh
rad --output json schema pull
```

If a modified desired schema blocks pull, inspect or commit it first. Use `--force` only when the user deliberately chooses the server copy; Rad backs up the local file before replacement.

## Diagnose and operate

Start with the read-only diagnostic:

```sh
rad --output json doctor
```

For online schema work:

```sh
rad --output json schema transitions list
rad --output json schema transitions get tr42
rad --output json schema transitions wait tr42
```

Cancel only when abandoning the requested schema change:

```sh
rad --non-interactive schema transitions cancel tr42 --yes
```

Read [references/operations.md](references/operations.md) when starting servers, choosing catalog or storage modes, diagnosing transitions, or recovering failures.

## Generate application clients

Configured clients regenerate after migrate and pull unless `--no-generate` is explicit. To regenerate the accepted schema directly:

```sh
rad --output json generate
```

Read [references/go-client.md](references/go-client.md) when configuring Go output, interpreting compatibility failures, or reviewing generated-code ownership.

## Use bundled contracts

Inspect the exact declarative schema format and CLI contract without relying on network access:

```sh
rad schema json-schema
rad spec --format json
```

Treat these bundled artifacts and the installed skill as authoritative for the running Rad version. Online documentation is at https://www.radengine.dev/docs.
