# Tracker — the RAD demo product

A team task tracker (accounts, teams, boards, tasks with subtasks, comments,
labels) built **entirely** on RAD's generated client. This directory is the
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
rad migrate  -f demo/schema.rad -d demo/data     # diff & reconcile the DB
   │
   ▼
rad generate -f demo/schema.rad -o demo/generated --pkg tracker
   │
   ▼
go build .                                       # compiler catches schema drift
   │
   ▼
typed app  →  QIR  →  planner  →  SlateDB  →  nested JSON
```

No SQL is written anywhere — not by the app, not by the tools.

Run it from the repo root (`task demo`) or here:

```
CGO_LDFLAGS="-L$PWD/../lib -Wl,-rpath,$PWD/../lib" go run .
```

The app migrates its own store on startup (the client embeds its schema),
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
