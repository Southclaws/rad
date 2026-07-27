// The Tracker walkthrough in TypeScript, using only the generated client —
// the same wire protocol, the same typed experience, no SQL. Run with a Rad
// server up (defaults to rad://localhost:7237):
//
//	node examples/demo/tracker-demo.ts
import { connect, isConflict } from "./generated/rad-client.generated.ts";

const db = connect(process.env.RAD_URL ?? "rad://localhost:7237");
await db.ping();

// Accounts (unique username enforced by the database).
const ada = await db.accounts.create({ username: "ada-ts", password_hash: "…" });
const grace = await db.accounts.create({
  username: "grace-ts",
  password_hash: "…",
});
try {
  await db.accounts.create({ username: "ada-ts", password_hash: "…" });
} catch (err) {
  console.log(`   duplicate username rejected: ${(err as Error).message}`);
}
const found = await db.accounts.byUsername("ada-ts");
console.log(`   byUsername: ${found?.username}`);

// A team, board, and tasks. Each write is its own execution program; a
// following create consumes the previous row's id.
const team = await db.teams.create({ name: "Radlabs TS" });
await db.teamMembers.create({
  team_id: team.id,
  user_id: ada.id,
  role: "owner",
});
const board = await db.boards.create({ team_id: team.id, name: "TS Launch" });
const ship = await db.tasks.create({
  board_id: board.id,
  title: "Ship the TS client",
  creator_id: ada.id,
  assignee_id: ada.id,
  priority: 1,
});
await db.tasks.create({
  board_id: board.id,
  title: "Write docs",
  creator_id: ada.id,
  parent_id: ship.id,
  assignee_id: grace.id,
});
await db.comments.create({
  task_id: ship.id,
  author_id: grace.id,
  body: "on it",
});
console.log(`   seeded team ${team.name} ✓`);

// The nested board view: one query, typed all the way down.
const view = await db.boards
  .query()
  .idEq(board.id)
  .includeTasks((t) =>
    t
      .orderByPriority()
      .includeAssignee()
      .includeComments((c) => c.orderById().includeAuthor()),
  )
  .first();

for (const task of view?.tasks ?? []) {
  const who = task.assignee?.username ?? "unassigned";
  const comments =
    task.comments
      ?.map((c) => `${c.author?.username}: "${c.body}"`)
      .join(", ") ?? "";
  console.log(`   [${task.priority}] ${task.title} → ${who} ${comments}`);
}

// Aggregates: board stats folded server-side, no rows fetched.
const onBoard = () => db.tasks.query().boardIdEq(board.id);
// One grouped fold answers the whole card in a single round trip.
const byStatus = await onBoard().countByStatus();
const totalTasks = byStatus.reduce((n, g) => n + g.count, 0);
const doneTasks = byStatus.find((g) => g.status === "done")?.count ?? 0;
console.log(
  `   ${board.name}: ${totalTasks} tasks · ${totalTasks - doneTasks} open · ${doneTasks} done (GROUP BY, one query)`,
);
const avgEst = await onBoard().avgEstimate(); // null when nothing estimated
console.log(
  `   avg estimate: ${avgEst === null ? "— (none yet)" : `${avgEst.toFixed(1)}h`}`,
);

// Typed filters, patch with clear-to-NULL, and a conflict retry.
const mine = await db.tasks.query().assigneeIdEq(ada.id).statusNe("done").all();
console.log(`   ada's open tasks: ${mine.length}`);

const done = await db.tasks.update(mine[0].id, {
  status: "done",
  assignee_id: null,
});
console.log(
  `   completed "${done?.title}", assignee cleared: ${done?.assignee_id === null}`,
);

// Read, decide, write: reassign after checking the current value.
const cur = await db.tasks.get(mine[0].id);
if (cur && cur.status !== "done") {
  await db.tasks.update(mine[0].id, { status: "doing" });
  console.log(`   "${cur.title}" set to doing ✓`);
}

console.log("done — TypeScript app, same wire, zero SQL.");
