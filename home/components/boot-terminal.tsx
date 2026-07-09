"use client";

import { useEffect, useState } from "react";

type Line =
  | { kind: "cmd"; text: string }
  | { kind: "out"; text: string }
  | { kind: "ok"; text: string };

const LINES: Line[] = [
  { kind: "cmd", text: "rad serve" },
  { kind: "out", text: "rad v0 — one binary: server · devtool · codegen" },
  { kind: "out", text: "storage    s3://rad-data  (durable · stateless)" },
  { kind: "out", text: "migrate    schema.rad → 9 tables, 0 changes" },
  { kind: "ok", text: "listening  rad://0.0.0.0:7237" },
  { kind: "out", text: "devtool    http://localhost:7237" },
];

// A terminal that boots. Ships with the full output rendered (SSR / no-JS /
// reduced-motion safe); on mount it replays the lines printing in sequence,
// then blinks the cursor.
export function BootTerminal() {
  const [shown, setShown] = useState(LINES.length);

  useEffect(() => {
    const reduce = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    if (reduce) return;

    setShown(0);
    let i = 0;
    const id = setInterval(() => {
      i += 1;
      setShown(i);
      if (i >= LINES.length) clearInterval(id);
    }, 260);
    return () => clearInterval(id);
  }, []);

  return (
    <div className="term crop" role="img" aria-label="Running `rad serve`: rad v0 boots as one binary — durable, stateless S3 storage, migrates schema.rad to 9 tables, and listens on rad://0.0.0.0:7237">
      <div className="term__bar" aria-hidden="true">
        <span className="term__dots">● ● ●</span>
        <span className="term__title">rad — serve</span>
      </div>
      <div className="term__body" aria-hidden="true">
        {LINES.map((line, idx) => (
          <div
            key={idx}
            className={`term__line term__${line.kind}`}
            style={{
              opacity: idx < shown ? 1 : 0,
              transition: "opacity 0.18s ease",
            }}
          >
            {line.text}
          </div>
        ))}
        <div className="term__line" style={{ opacity: shown >= LINES.length ? 1 : 0 }}>
          <span className="term__cursor" />
        </div>
      </div>
    </div>
  );
}
