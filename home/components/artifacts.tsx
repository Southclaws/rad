"use client";

import { Fragment, useState } from "react";

type Lang = "json";

const FILES: { name: string; lang: Lang; code: string }[] = [
  {
    name: "query.lir.json",
    lang: "json",
    code: `{
  "nodes": {
    "users": { "kind": "scan", "table": "users", "scope": "u" },
    "users_ordered": {
      "kind": "order",
      "input": "users",
      "terms": [
        { "expr": { "kind": "col", "scope": "u", "column": "id" } }
      ]
    },
    "posts": { "kind": "scan", "table": "posts", "scope": "p" },
    "posts_for_user": {
      "kind": "filter",
      "input": "posts",
      "predicate": {
        "kind": "binary",
        "op": "eq",
        "left": { "kind": "col", "scope": "p", "column": "user_id" },
        "right": { "kind": "col", "scope": "u", "column": "id" }
      }
    },
    "post_objects": {
      "kind": "project",
      "input": "posts_for_user",
      "scope": "po",
      "fields": [
        { "as": "id", "expr": { "kind": "col", "scope": "p", "column": "id" } },
        { "as": "title", "expr": { "kind": "col", "scope": "p", "column": "title" } }
      ]
    },
    "posts_ordered": {
      "kind": "order",
      "input": "post_objects",
      "terms": [
        { "expr": { "kind": "col", "scope": "po", "column": "id" } }
      ]
    },
    "result": {
      "kind": "project",
      "input": "users_ordered",
      "fields": [
        { "as": "id", "expr": { "kind": "col", "scope": "u", "column": "id" } },
        { "as": "name", "expr": { "kind": "col", "scope": "u", "column": "name" } },
        { "as": "posts", "expr": { "kind": "array", "node": "posts_ordered" } }
      ]
    }
  },
  "root": { "node": "result", "cardinality": "many" }
}`,
  },
  {
    name: "transaction.pir.json",
    lang: "json",
    code: `{
  "statements": [
    {
      "name": "author",
      "kind": "create",
      "table": "users",
      "relation": {
        "nodes": {
          "row": {
            "kind": "rows",
            "scope": "r",
            "columns": [
              { "name": "id", "type": "text" },
              { "name": "name", "type": "text" }
            ],
            "rows": [["u1", "Ada"]]
          }
        },
        "root": { "node": "row", "cardinality": "many" }
      }
    },
    {
      "name": "post",
      "kind": "create",
      "table": "posts",
      "relation": {
        "nodes": {
          "author": { "kind": "ref", "binding": "author", "scope": "a" },
          "post": {
            "kind": "project",
            "input": "author",
            "fields": [
              { "as": "id", "expr": { "kind": "lit", "value": { "type": "text", "value": "p1" } } },
              { "as": "user_id", "expr": { "kind": "col", "scope": "a", "column": "id" } },
              { "as": "title", "expr": { "kind": "lit", "value": { "type": "text", "value": "Relational values" } } }
            ]
          }
        },
        "root": { "node": "post", "cardinality": "many" }
      }
    }
  ],
  "result": "post"
}`,
  },
];

const KEYWORDS: Record<Lang, RegExp> = {
  json: /\b(true|false|null|many|exactly_one|scan|filter|project|order|array|create|query)\b/g,
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
      const isKey = parts[i + 1]?.trimStart().startsWith(":");
      nodes.push(
        <span className={isKey ? "t-key" : "t-str"} key={`s${i}`}>
          {part}
        </span>,
      );
      return;
    }
    const commentIdx = part.indexOf("//");
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
      if (m.index > last)
        nodes.push(
          <Fragment key={`t${i}-${last}`}>
            {code.slice(last, m.index)}
          </Fragment>,
        );
      nodes.push(
        <span className="t-key" key={`k${i}-${m.index}`}>
          {m[0]}
        </span>,
      );
      last = m.index + m[0].length;
    }
    if (last < code.length)
      nodes.push(<Fragment key={`e${i}-${last}`}>{code.slice(last)}</Fragment>);
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
      <div
        className="filetab"
        role="tablist"
        aria-label="Intermediate representation examples"
      >
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
