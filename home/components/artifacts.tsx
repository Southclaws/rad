"use client";

import { Fragment, useState } from "react";

type Lang = "yaml" | "go" | "ts";

const FILES: { name: string; lang: Lang; code: string }[] = [
  {
    name: "schema.rad",
    lang: "yaml",
    code: `tables:
  - name: boards
    columns:
      - { name: id,      type: string, pk: true, default: uuid() }
      - { name: team_id, type: string, ref: teams.id }
      - { name: name,    type: string }

  - name: tasks
    columns:
      - { name: id,          type: string, pk: true, default: uuid() }
      - { name: board_id,    type: string, ref: boards.id, index: true }
      - { name: title,       type: string }
      - { name: status,      type: string, default: todo }
      - { name: assignee_id, type: string, ref: users.id, nullable: true }
      - { name: estimate,    type: float64, nullable: true }
    indexes:
      - { columns: [board_id, status] }`,
  },
  {
    name: "client.go",
    lang: "go",
    code: `db, _ := tracker.Connect("rad://localhost:7237")

// one query, typed all the way down — no SQL, no N+1
board, _, _ := db.Boards.Query().
    IDEq(id).
    IncludeTasks(func(t *tracker.TaskInclude) {
        t.OrderByPriority().IncludeAssignee()
    }).
    First(ctx)

// aggregates fold server-side — zero rows fetched
open, _ := db.Tasks.Query().
    BoardIDEq(id).StatusNe("done").Count(ctx)`,
  },
  {
    name: "client.ts",
    lang: "ts",
    code: `const db = connect("rad://localhost:7237");

// the same wire, the same shapes, from TypeScript
const board = await db.boards.query()
  .idEq(id)
  .includeTasks((t) => t.orderByPriority().includeAssignee())
  .first();

const open = await db.tasks.query()
  .boardIdEq(id).statusNe("done").count();`,
  },
];

const KEYWORDS: Record<Lang, RegExp> = {
  yaml: /\b(string|int64|float64|bool|true|false|uuid|now_ms)\b/g,
  go: /\b(func|db|ctx|nil|await|const)\b/g,
  ts: /\b(const|await|db|connect)\b/g,
};

// Minimal, dependency-free highlighter. Strings and comments carry most of the
// signal; keywords get a light green touch. It splits each line on quoted
// strings first, so a URL's "//" inside a string is never mistaken for a
// comment.
function highlight(line: string, lang: Lang, key: number) {
  const nodes: React.ReactNode[] = [];
  const parts = line.split(/("[^"]*")/g);
  parts.forEach((part, i) => {
    if (part.startsWith('"')) {
      nodes.push(
        <span className="t-str" key={`s${i}`}>
          {part}
        </span>,
      );
      return;
    }
    const commentIdx = lang === "yaml" ? part.indexOf("#") : part.indexOf("//");
    let code = part;
    let comment = "";
    if (commentIdx >= 0) {
      code = part.slice(0, commentIdx);
      comment = part.slice(commentIdx);
    }
    const kw = KEYWORDS[lang];
    let last = 0;
    let m: RegExpExecArray | null;
    kw.lastIndex = 0;
    while ((m = kw.exec(code))) {
      if (m.index > last) nodes.push(<Fragment key={`t${i}-${last}`}>{code.slice(last, m.index)}</Fragment>);
      nodes.push(
        <span className="t-key" key={`k${i}-${m.index}`}>
          {m[0]}
        </span>,
      );
      last = m.index + m[0].length;
    }
    if (last < code.length) nodes.push(<Fragment key={`e${i}-${last}`}>{code.slice(last)}</Fragment>);
    if (comment)
      nodes.push(
        <span className="t-com" key={`c${i}`}>
          {comment}
        </span>,
      );
  });
  return (
    <div className="term__line" key={key}>
      {nodes.length ? nodes : " "}
    </div>
  );
}

export function Artifacts() {
  const [active, setActive] = useState(0);
  const file = FILES[active];

  return (
    <div className="panel crop" style={{ borderRadius: 8, overflow: "hidden" }}>
      <div className="filetab" role="tablist" aria-label="Generated artifacts">
        {FILES.map((f, i) => (
          <button
            key={f.name}
            role="tab"
            aria-selected={i === active}
            onClick={() => setActive(i)}
          >
            {f.name}
          </button>
        ))}
      </div>
      <pre className="code" aria-label={`${file.name} source`}>
        {file.code.split("\n").map((line, i) => highlight(line, file.lang, i))}
      </pre>
    </div>
  );
}
