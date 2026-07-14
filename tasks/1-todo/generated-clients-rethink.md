# Rethink the generated-client world

Status: placeholder — deliberately unscoped. During the PIR arc
(data-mutation-and-transaction-protocol.md) generated clients get the
*minimum* port: keep them compiling, keep the Tracker demo running, no
redesign.

The rethink, when it happens, owns:

- The program-construction model: `Txn(fn)`'s callback becomes
  *construction*, not live interaction — callback code cannot branch on
  actual query results because nothing has executed yet. Conditions must
  either be relational (in PIR) or split across programs. The name itself
  (`Program` / `Atomic` / `Execute`?) and whether a callback is even the
  right shape are part of this.
- How typed table handles compose with programs, statement-result
  bindings, and the `rows` constant relation (bulk input ergonomics).
- Go/TS parity, and whatever `rad schema pull`-driven generation implies
  for direct-mode databases (tasks/3-done/direct-catalog-mode.md).

Blocked on: the PIR cutover landing first.
