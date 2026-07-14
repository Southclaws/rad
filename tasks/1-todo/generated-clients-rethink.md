# Rethink the generated-client world (and finish the PIR cutover)

Status: ready — unblocked. PIR shipped (stages 1–6 of
data-mutation-and-transaction-protocol.md): `/execute` is the canonical
execution path and the whole battle corpus runs through it. The generated
clients still speak the legacy CRUD + `/tx` endpoints, which is why those
endpoints remain. This task migrates them and then deletes the legacy
surface — PIR-cutover stage 7 lives here, because it turned out to be a
client-model change, not an endpoint deletion.

The rethink owns:

- **The program-construction model.** A typed client builds a PIR program
  and submits it once; the callback is *construction*, not live
  interaction. Create-chains work through statement-result refs
  (`tx.Teams.Create` → `tx.Boards.Create(team.ID)` becomes a create
  statement whose input refs the team statement). The name/shape
  (`Program` / `Atomic` / `Execute`? is a callback even right?) is part of
  this.
- **Typed single-row CRUD → PIR.** The generated client knows its schema,
  so it can build the typed `rows`/PK relations a create/update/delete
  program needs — the thing the hand-written `radclient.Create(map)`
  cannot do without a schema round-trip. This is what lets `/create`,
  `/update`, `/delete` be deleted.
- **Which interactive-transaction subset survives.** Read-branch-write
  (read a value, branch in application code, then write, atomically) is a
  genuine interactive case the ADR reserves `/tx` for — it is not a
  submit-once program. Decide whether the client keeps a thin interactive
  transaction for it or the pattern moves to multiple `/execute` calls.
  Only once that is settled can the `/tx` tree + `sessions.go` be deleted
  (or deliberately kept as the minimal interactive surface).
- How typed table handles compose with programs, statement-result
  bindings, and the `rows` constant relation (bulk input ergonomics).
- Go/TS parity, and whatever `rad schema pull`-driven generation implies
  for direct-mode databases (tasks/3-done/direct-catalog-mode.md).

## The deletion, once the clients migrate

- `radclient` autocommit CRUD and the `oas` operations `RowCreate`,
  `RowUpdate`, `RowDelete`, `Query`; the `/create`, `/update`, `/delete`,
  `/query` paths.
- The `/tx` tree + `rad/server/api/sessions.go` — unless a read-branch-
  write interactive transaction is deliberately kept.
- dbserver_test's transaction cases become program cases.
- No compatibility aliases; `/execute` is the one data-plane entry.

Related: tasks/1-todo/data-mutation-and-transaction-protocol.md (PIR, done),
tasks/3-done/direct-catalog-mode.md.
