# simple task tracker

A team task tracker (accounts, teams, boards, tasks with subtasks, comments,
labels) built **entirely** on Rad's generated client. This directory is the
proof of rad's developer experience:

```
rad.schema.yaml   the product's data model (YAML, JSON-Schema validated)
rad.config.yaml   the target Rad database
rad.state/        CLI-managed migration state
generated/        `rad_client_gen.go` and `rad-client.generated.ts` (do not edit)
main.go           the application — imports only ./generated
```
