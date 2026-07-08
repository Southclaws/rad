// The Tracker walkthrough in TypeScript, using only the generated client —
// the same wire protocol, the same typed experience, no SQL. Run with a RAD
// server up (defaults to rad://localhost:7237):
//
//	node demo/tracker-demo.ts
import { connect, isConflict } from "./generated/tracker.ts";

const db = connect(process.env.RAD_URL ?? "rad://localhost:7237");
await db.ping();

const steps = await db.migrate();
console.log(`── migrate: ${steps.length} schema steps applied`);

// Accounts (unique username enforced by the database).
const ada = await db.users.create({ username: "ada-ts", password_hash: "…" });
const grace = await db.users.create({ username: "grace-ts", password_hash: "…" });
try {
  await db.users.create({ username: "ada-ts", password_hash: "…" });
} catch (err) {
  console.log(`   duplicate username rejected: ${(err as Error).message}`);
}
const found = await db.users.byUsername("ada-ts");
console.log(`   byUsername: ${found?.username}`);

// A team, board, and tasks — atomically.
const { team, board } = await db.tx(async (tx) => {
  const team = await tx.teams.create({ name: "Radlabs TS" });
  await tx.teamMembers.create({ team_id: team.id, user_id: ada.id, role: "owner" });
  const board = await tx.boards.create({ team_id: team.id, name: "TS Launch" });
  const ship = await tx.tasks.create({
    board_id: board.id, title: "Ship the TS client", creator_id: ada.id,
    assignee_id: ada.id, priority: 1,
  });
  await tx.tasks.create({
    board_id: board.id, title: "Write docs", creator_id: ada.id,
    parent_id: ship.id, assignee_id: grace.id,
  });
  await tx.comments.create({ task_id: ship.id, author_id: grace.id, body: "on it" });
  return { team, board };
});
console.log(`   seeded team ${team.name} atomically ✓`);

// The nested board view: one query, typed all the way down.
const view = await db.boards
  .query()
  .idEq(board.id)
  .includeTasks((t) =>
    t.orderByPriority().includeAssignee().includeComments((c) => c.includeAuthor()),
  )
  .first();

for (const task of view?.tasks ?? []) {
  const who = task.assignee?.username ?? "unassigned";
  const comments = task.comments?.map((c) => `${c.author?.username}: "${c.body}"`).join(", ") ?? "";
  console.log(`   [${task.priority}] ${task.title} → ${who} ${comments}`);
}

// Typed filters, patch with clear-to-NULL, and a conflict retry.
const mine = await db.tasks.query().assigneeIdEq(ada.id).statusNe("done").all();
console.log(`   ada's open tasks: ${mine.length}`);

const done = await db.tasks.update(mine[0].id, { status: "done", assignee_id: null });
console.log(`   completed "${done?.title}", assignee cleared: ${done?.assignee_id === null}`);

try {
  await db.tx(async (tx) => {
    await tx.tasks.update(mine[0].id, { status: "doing" });
    await db.tasks.update(mine[0].id, { status: "todo" }); // rival commits first
  });
} catch (err) {
  console.log(`   conflict detected and retryable: ${isConflict(err)}`);
}

console.log("done — TypeScript app, same wire, zero SQL.");
