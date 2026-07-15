# Rethink the generated-client world

Status: ready. The PIR cutover is DONE
(tasks/3-done/data-mutation-and-transaction-protocol.md): `/execute` is the
only data endpoint, the legacy CRUD + `/tx` surface is deleted, and the
generated Go and TS clients already run entirely over `/execute`
(autocommit single-statement programs, no interactive transactions). So the
*deletion* this task once owned is done. What remains is ergonomics —
making the program model first-class rather than one-statement-at-a-time —
and the interactive-transaction decision.

The rethink owns:

- **A first-class program-building API.** Today each generated call is a
  separate one-statement `/execute` program (autocommit), so a multi-write
  workflow is several round-trips and loses cross-write atomicity. The
  rethink gives the client a way to *build one program* — several
  statements, submitted once, atomic — with create-chains expressed through
  statement-result refs (`teams.Create` → `boards.Create` referencing the
  team statement's result relation, not a Go value). The shape and name
  (`Program` / `Atomic` / builder vs callback — and note a callback is
  construction, not live interaction, so it cannot branch on results) is
  the core question.
- **Whether to reintroduce a narrow interactive transaction.** The cutover
  deleted `/tx`, which removed atomic read-decide-write and optimistic
  conflict/retry across application logic (the demos' old showcase). Those
  are genuine interactive cases the ADR reserves the right to bring back.
  Decide: express them as a single program where possible (a conditional
  update whose input filters relationally), accept multiple non-atomic
  `/execute` calls, or reintroduce a minimal server-held transaction for
  the read-decide-write case — and if so, on what contract.
- **Bulk-input ergonomics**: how typed table handles compose with the
  `rows` constant relation for batch create/update, and with
  statement-result bindings.
- Go/TS parity, and whatever `rad schema pull`-driven generation implies
  for direct-mode databases (tasks/3-done/direct-catalog-mode.md).

Related: tasks/3-done/data-mutation-and-transaction-protocol.md (PIR, done),
tasks/3-done/direct-catalog-mode.md, [[codegen-generator-rewrite]] (owns the
generator machinery + emitted builder surface; this task owns the runtime
program model — reconcile the ergonomic layer between them).
