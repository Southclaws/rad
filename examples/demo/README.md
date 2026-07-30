# simple task tracker

A team task tracker (accounts, teams, boards, tasks with subtasks, comments,
labels) built **entirely** on Rad's generated client. This directory is the
proof of rad's developer experience:

```
rad.schema.yaml   the product's data model (YAML, JSON-Schema validated)
rad.config.yaml   the target Rad database
rad.state/        CLI-managed migration state
generated/        `rad_client_gen.go` (generated; do not edit)
main.go           the application — imports only ./generated
lifecycle.go      cross-platform local server orchestration for `task demo`
```

Run `task demo` from the repository root for a fresh local database and the
complete outside-in tracker workflow. To run the application against an
already-running server instead, use `RAD_URL=rad://host:7237 go run
./examples/demo`.
