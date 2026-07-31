# Operations reference

## Contents

- Server startup
- Catalog modes
- Storage backends
- Diagnostics
- Transition lifecycle
- Recovery rules

## Server startup

Local schema-managed development:

```sh
rad serve --storage memory --catalog-mode schema
```

File storage persists under `--db`; S3 storage requires `--s3-bucket` and accepts region, prefix, and custom endpoint settings. The public API defaults to port 7237 and the administration UI uses the following port.

Run `rad serve --help` for the exact environment-variable mapping shipped with the binary.

## Catalog modes

- `schema`: `rad.schema.yaml` migrations own catalog changes; direct catalog writes are rejected.
- `direct`: both imperative catalog operations and desired-schema migration are available.

The mode is fixed when a fresh database initializes. Reopening existing storage with a different explicit mode is an error.

## Diagnostics

Run:

```sh
rad --output json doctor
```

Doctor checks configuration, desired schema, accepted local state, server health and identity, desired diff, generated targets, and retained schema-transition records. Treat failures as blockers. Warnings commonly identify an unapplied desired schema, missing initial accepted state, or a client needing regeneration.

Use `rad schema status` for the smaller server, accepted, desired, and generated-client comparison.

## Transition lifecycle

Transition states are `waiting`, `building`, `catching_up`, `validating`, `ready`, `failed`, and `cancelled`. The last three are terminal. Progress counters are advisory; the durable state and `last_error` are authoritative.

```sh
rad --output json schema transitions list
rad --output json schema transitions list --state failed
rad --output json schema transitions get tr42
rad --output json schema transitions wait tr42 --timeout-seconds 600
```

`degraded` retained-work pressure warrants investigation. `write_gated` means affected writes can be rejected until work catches up or terminates.

Cancel only when abandoning a requested change. Cancellation invalidates worker ownership, removes foreground obligations, and schedules partial artifacts for cleanup:

```sh
rad --non-interactive schema transitions cancel tr42 --yes
```

Do not cancel `ready` work; change or remove the logical object through the desired schema. Fix the reported cause of `failed` work, then plan again.

## Recovery rules

- Catalog conflict: run diff again against the new current version.
- Server ahead of local accepted state: pull, then reapply intended desired edits.
- Missing or invalid local accepted state with a correct server: pull.
- Desired file contains valuable local edits: do not force pull until those edits are preserved.
- Failed online transition: preserve `transition_id` and `last_error`, fix data or target, and replan.
- Generation fails after schema commit: the database and accepted state remain advanced; fix generation and run `rad generate`.
- Schema history divergence at the same version: stop and inspect both histories. Do not force a migration over it.
