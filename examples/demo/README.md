# Tracker — the Rad demo product

A team task tracker (accounts, teams, boards, tasks with subtasks, comments,
labels) built **entirely** on Rad's generated client. This directory is the
proof of the developer experience:

```
schema.rad          the product's data model (YAML, JSON-Schema validated)
generated/          typed Go client emitted by `rad generate` (do not edit)
main.go             the application — imports only ./generated
```

## The workflow this proves

```
edit schema.rad
   │
   ▼
rad migrate  -u rad://localhost -f examples/demo/schema.rad   # reconcile the server's DB
   │
   ▼
rad generate -f examples/demo/schema.rad -o examples/demo/generated --pkg tracker
   │
   ▼
go build .                                           # compiler catches schema drift
   │
   ▼
typed app  →  rad:// wire LIR  →  planner  →  SlateDB  →  nested JSON
```

The app is pure Go — no cgo, no native library; only the server needs
SlateDB. Point it anywhere with RAD_URL (default rad://localhost:7237).

No SQL is written anywhere — not by the app, not by the tools.

Run it from the repo root (`task demo` starts a fresh server and the app),
or against any running Rad server:

```
RAD_URL=rad://your-server go run .
```

The app migrates the remote database on startup (the client embeds its
schema),
then walks through: signup/login with a unique username index, an atomic
multi-table seed transaction, a three-level nested board view (tasks →
assignee/comments→author/labels), typed queries (filters, ordering,
pagination, IS NULL), patches with clear-to-NULL, delete-restrict foreign
keys, and an optimistic-concurrency conflict with a retry.

## What the schema exercises

uuid/unix_ms/email formats · generator + literal defaults · nullable
columns and nullable FKs · two FKs from tasks to users (assignee/creator →
`TasksByAssignee`/`TasksByCreator`) · a self-referential FK (subtasks) ·
composite PKs (join tables) · composite unique constraints · composite
indexes · many-to-many via task_labels.

## Schema evolution, demonstrated in git history

The commit "Evolve the Tracker schema" shows the loop against a live,
populated database: a column add, a rename (via `renamed_from` — no data
rewrite, rows are keyed by column ID), and a backfilled composite index.
Regenerating the client made the app's single use of the renamed field a
compile error; one line later everything ran.
