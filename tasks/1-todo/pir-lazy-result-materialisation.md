# PIR: don't materialise unreferenced statement results

Status: todo — a known cost, surfaced by the PIR schema review (2026-07-14).
Small, self-contained.

The PIR schema now permits it (Program `# Model`): "the engine need not
construct or retain [a statement's result relation] when it is neither
referenced by a later statement nor selected as the program result — an
unreferenced 100,000-row delete may keep only its affected count." The
engine does not yet take that license.

## Current behaviour

`exec.runProgram` (rad/engine/05_exec/program.go) drains every statement's
frames and stores them in the `program` map under the statement name,
referenced or not. So a query-driven delete or update over 100k rows holds
100k pre-/post-image frames in memory even when nothing consumes them and
the program result is a different statement — pure waste, and a real memory
ceiling on large mutations.

## The fix

Before executing, compute which statement names are actually needed:
- the program's `result` statement, and
- every statement referenced by a `ref` in any later statement's plan
  (the binder/planner already resolves these — `planner.BindProgram` knows
  the ref graph, or scan each `PhysPlan` for `RefExec.Binding`).

For a statement whose result is not needed, still execute it (effects and
constraint checks must run), but don't retain its frames — keep only the
affected count for the summary. Mutations already know their affected count
without holding the rows; the drain-and-store is the only reason the frames
linger.

Keep it a pure optimisation: results that ARE referenced or selected behave
exactly as today (value semantics, produced once, never replayed). Add a
test that a large unreferenced mutation statement in a multi-statement
program does not retain its rows (e.g. affected count correct, and — if a
counting/memory probe is feasible — frames not held).

Related: tasks/3-done/data-mutation-and-transaction-protocol.md (PIR),
tasks/1-todo/query-trace.md (per-statement affected counts live here too).
