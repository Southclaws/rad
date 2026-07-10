// Schematic diagrams for the QIR spec, in the home page's hairline / phosphor
// style (see DESIGN.md; matches components/blog-diagrams.tsx). Provided to MDX
// via the docs components map. Colours are DESIGN.md tokens; literal path bytes
// read muted, order-preserving encoded tuples read green.

import type { ReactNode } from "react";

// A downward arrow ending at (x, y2), drawn from y1. Green hairline.
function Down({ x, y1, y2 }: { x: number; y1: number; y2: number }) {
  return (
    <g stroke="var(--green-mid)" strokeWidth="1.5" strokeLinecap="round">
      <line x1={x} y1={y1} x2={x} y2={y2} />
      <line x1={x} y1={y2} x2={x - 5} y2={y2 - 7} />
      <line x1={x} y1={y2} x2={x + 5} y2={y2 - 7} />
    </g>
  );
}

// A straight arrow from (x1,y1) to (x2,y2) with a small head at the end.
function Arrow({
  x1,
  y1,
  x2,
  y2,
  dash,
}: {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  dash?: boolean;
}) {
  const a = Math.atan2(y2 - y1, x2 - x1);
  const h = 7;
  return (
    <g
      stroke="var(--green-mid)"
      strokeWidth="1.4"
      strokeLinecap="round"
      fill="none"
    >
      <line
        x1={x1}
        y1={y1}
        x2={x2}
        y2={y2}
        strokeDasharray={dash ? "4 5" : undefined}
      />
      <line
        x1={x2}
        y1={y2}
        x2={x2 - h * Math.cos(a - 0.5)}
        y2={y2 - h * Math.sin(a - 0.5)}
      />
      <line
        x1={x2}
        y1={y2}
        x2={x2 - h * Math.cos(a + 0.5)}
        y2={y2 - h * Math.sin(a + 0.5)}
      />
    </g>
  );
}

function Fig({
  label,
  caption,
  viewBox,
  children,
}: {
  label: string;
  caption: ReactNode;
  viewBox: string;
  children: ReactNode;
}) {
  return (
    <figure className="fig">
      <svg viewBox={viewBox} fill="none" role="img" aria-label={label}>
        {children}
      </svg>
      <figcaption>{caption}</figcaption>
    </figure>
  );
}

// ── the read pipeline: wire QIR down to KV and back up as JSON ──────────────
export function QIRPipeline() {
  const stages: {
    label: string;
    sub: string;
    tone?: "hero" | "kv";
  }[] = [
    { label: "generated client", sub: "builds a protocol.Read" },
    { label: "POST /query", sub: "the rad:// wire · protocol.Read", tone: "hero" },
    { label: "wireconv.toRead", sub: "coerce JSON to column types" },
    { label: "planner.PlanRead", sub: "choose access path · validate" },
    {
      label: "exec.runShapedRead",
      sub: "fetch · filter · sort · include · fold",
    },
    {
      label: "01_kv over SlateDB",
      sub: "Get · Scan[start, end) · Put · Delete",
      tone: "kv",
    },
    { label: "frontend.RecordsJSON", sub: "nested records, shaped like your data" },
  ];
  const W = 340;
  const X = 150;
  const H = 54;
  const GAP = 34;
  const top = 6;
  const cx = X + W / 2;
  return (
    <Fig
      viewBox={`0 0 640 ${top + stages.length * (H + GAP)}`}
      label="A vertical pipeline: the generated client builds a protocol.Read, which travels over POST /query, through wireconv, the planner, the executor, down to the KV store over SlateDB, and back up as nested JSON."
      caption="One read, top to bottom: the QIR is chosen at the client, lowered, planned, executed against the KV store, and reassembled as nested JSON."
    >
      {stages.map((s, i) => {
        const y = top + i * (H + GAP);
        const hero = s.tone === "hero";
        const kv = s.tone === "kv";
        const stroke = hero
          ? "var(--green)"
          : kv
            ? "var(--green-mid)"
            : "var(--line-2)";
        return (
          <g key={s.label}>
            {i > 0 && <Down x={cx} y1={y - GAP} y2={y - 4} />}
            <rect
              x={X}
              y={y}
              width={W}
              height={H}
              rx="7"
              stroke={stroke}
              strokeWidth={hero ? 1.8 : 1.5}
              fill={hero ? "var(--green-deep)" : "none"}
            />
            <text
              x={cx}
              y={y + 23}
              textAnchor="middle"
              fontSize="14.5"
              fontWeight={hero ? 700 : 500}
              fill={hero ? "var(--green)" : "var(--ink)"}
            >
              {s.label}
            </text>
            <text
              x={cx}
              y={y + 42}
              textAnchor="middle"
              fontSize="11.5"
              fill={hero || kv ? "var(--green-mid)" : "var(--faint)"}
            >
              {s.sub}
            </text>
          </g>
        );
      })}
    </Fig>
  );
}

// ── physical key layout: how a row and an index entry are keyed ─────────────
export function KeyLayoutDiagram() {
  // kind: "lit" = literal ASCII path, "id" = a stable id, "enc" = order-preserving tuple
  type Seg = { t: string; w: number; kind: "lit" | "id" | "enc" };
  const rowKey: Seg[] = [
    { t: "/rad/data/", w: 116, kind: "lit" },
    { t: "{table_id}", w: 104, kind: "id" },
    { t: "/primary/", w: 104, kind: "lit" },
    { t: "{pk_tuple}", w: 156, kind: "enc" },
  ];
  const idxKey: Seg[] = [
    { t: "/rad/index/", w: 116, kind: "lit" },
    { t: "{table_id}", w: 92, kind: "id" },
    { t: "/{index_id}/", w: 112, kind: "lit" },
    { t: "{indexed_tuple}", w: 150, kind: "enc" },
    { t: "{pk_tuple}", w: 110, kind: "enc" },
  ];
  const stroke = (k: Seg["kind"]) =>
    k === "enc" ? "var(--green-mid)" : k === "id" ? "var(--line-2)" : "var(--line)";
  const textFill = (k: Seg["kind"]) =>
    k === "enc" ? "var(--green)" : k === "id" ? "var(--ink)" : "var(--muted)";

  function Bar({ segs, y }: { segs: Seg[]; y: number }) {
    let x = 24;
    return (
      <g>
        {segs.map((s) => {
          const el = (
            <g key={s.t}>
              <rect
                x={x}
                y={y}
                width={s.w}
                height="46"
                rx="4"
                stroke={stroke(s.kind)}
                strokeWidth="1.4"
                fill={s.kind === "enc" ? "var(--green-deep)" : "none"}
                fillOpacity={s.kind === "enc" ? 0.45 : 1}
              />
              <text
                x={x + s.w / 2}
                y={y + 28}
                textAnchor="middle"
                fontSize="12.5"
                fill={textFill(s.kind)}
              >
                {s.t}
              </text>
            </g>
          );
          x += s.w;
          return el;
        })}
      </g>
    );
  }

  return (
    <Fig
      viewBox="0 0 640 320"
      label="Two key formats. A row record key is /rad/data/, then the table id, then /primary/, then the encoded primary-key tuple, mapping to the row's JSON value. An index entry key is /rad/index/, the table id, the index id, the encoded indexed-column tuple, and the encoded primary-key tuple, mapping to the primary-key tuple as its value."
      caption="Literal path bytes (muted) frame the two identifiers and the order-preserving encoded tuples (green). An index entry's value is the PK tuple — a pointer back to the row."
    >
      <text x="24" y="22" fontSize="12.5" fill="var(--faint)">
        row record
      </text>
      <Bar segs={rowKey} y={32} />
      <Down x={70} y1={82} y2={98} />
      <rect
        x="24"
        y="100"
        width="520"
        height="38"
        rx="4"
        stroke="var(--line-2)"
        strokeWidth="1.3"
      />
      <text x="42" y="124" fontSize="12.5" fill="var(--muted)">
        JSON keyed by column id ·{" "}
        <tspan fill="var(--ink)">{`{ "c2": {…}, "c3": {…} }`}</tspan>
      </text>

      <text x="24" y="182" fontSize="12.5" fill="var(--faint)">
        index entry
      </text>
      <Bar segs={idxKey} y={192} />
      <Down x={70} y1={242} y2={258} />
      <rect
        x="24"
        y="260"
        width="320"
        height="38"
        rx="4"
        fill="var(--green-deep)"
        fillOpacity="0.4"
        stroke="var(--green-mid)"
        strokeWidth="1.3"
      />
      <text x="42" y="284" fontSize="12.5" fill="var(--green)">
        {`{pk_tuple}`} → a pointer back to the row
      </text>
    </Fig>
  );
}

// ── access-path selection: the filter picks the cheapest path ───────────────
export function AccessPathDiagram() {
  const paths: {
    rank: string;
    title: string;
    when: string;
    cost: string;
    x: number;
  }[] = [
    {
      rank: "1",
      title: "PK lookup",
      when: "all PK cols = literal",
      cost: "1 Get",
      x: 24,
    },
    {
      rank: "2",
      title: "index scan",
      when: "longest gapless prefix",
      cost: "1 Scan + N Get",
      x: 234,
    },
    {
      rank: "3",
      title: "full scan",
      when: "no usable equality",
      cost: "Scan every row",
      x: 444,
    },
  ];
  return (
    <Fig
      viewBox="0 0 640 292"
      label="A filter is reduced to its equality predicates, then chooseAccess ranks three paths: a complete primary-key equality set becomes a single-Get PK lookup; the longest gapless leading index prefix becomes an index scan of one Scan plus N Gets; otherwise a full table scan."
      caption="chooseAccess reads only column = literal equalities under top-level ANDs. Precedence: complete PK → longest gapless index prefix → full scan. The full filter is still re-checked on every fetched row."
    >
      <rect
        x="200"
        y="18"
        width="240"
        height="56"
        rx="7"
        stroke="var(--green)"
        strokeWidth="1.8"
        fill="var(--green-deep)"
      />
      <text
        x="320"
        y="41"
        textAnchor="middle"
        fontSize="14.5"
        fontWeight="700"
        fill="var(--green)"
      >
        chooseAccess(filter)
      </text>
      <text x="320" y="60" textAnchor="middle" fontSize="11.5" fill="var(--green-mid)">
        equalities() under top-level ANDs
      </text>
      {paths.map((p) => {
        const bx = p.x + 86;
        return (
          <g key={p.title}>
            <Arrow x1={320} y1={74} x2={bx} y2={150} />
            <rect
              x={p.x}
              y="150"
              width="172"
              height="118"
              rx="6"
              stroke="var(--line-2)"
              strokeWidth="1.5"
            />
            <circle
              cx={p.x + 22}
              cy={172}
              r="11"
              stroke="var(--green-mid)"
              strokeWidth="1.3"
            />
            <text
              x={p.x + 22}
              y={176}
              textAnchor="middle"
              fontSize="12"
              fill="var(--green)"
            >
              {p.rank}
            </text>
            <text
              x={p.x + 42}
              y={177}
              fontSize="14"
              fontWeight={600}
              fill="var(--ink)"
            >
              {p.title}
            </text>
            <text x={p.x + 16} y={210} fontSize="10.5" fill="var(--faint)">
              {p.when}
            </text>
            <line
              x1={p.x + 16}
              y1={224}
              x2={p.x + 156}
              y2={224}
              stroke="var(--line)"
              strokeWidth="1"
            />
            <text x={p.x + 16} y={247} fontSize="12.5" fill="var(--green-mid)">
              {p.cost}
            </text>
          </g>
        );
      })}
    </Fig>
  );
}

// ── an index scan is one Scan then a Get per matching entry ─────────────────
export function IndexScanDiagram() {
  const rows = [0, 1, 2];
  const iy = (i: number) => 70 + i * 52;
  return (
    <Fig
      viewBox="0 0 620 260"
      label="An index scan: one range Scan over the index prefix yields N index entries, each holding a primary-key tuple; the executor then issues one Get per PK against the data keyspace to load the rows."
      caption="An index stores PK pointers. One Scan collects the matching entries; then it is one Get per entry to fetch the base rows — N+1 by design, not a merge join."
    >
      <text x="30" y="40" fontSize="12.5" fill="var(--green-mid)">
        1 Scan · /rad/index/…/{"{prefix}"}
      </text>
      <rect
        x="30"
        y="52"
        width="228"
        height="176"
        rx="6"
        stroke="var(--green-mid)"
        strokeWidth="1.5"
        strokeDasharray="5 5"
      />
      {rows.map((i) => (
        <g key={i}>
          <rect
            x="46"
            y={iy(i)}
            width="196"
            height="36"
            rx="4"
            stroke="var(--line-2)"
            strokeWidth="1.3"
          />
          <text x="60" y={iy(i) + 23} fontSize="11.5" fill="var(--muted)">
            {"{indexed_tuple}"} ·{" "}
            <tspan fill="var(--green)">pk{i + 1}</tspan>
          </text>
          <Arrow x1={242} y1={iy(i) + 18} x2={372} y2={iy(i) + 18} />
          <rect
            x="372"
            y={iy(i)}
            width="216"
            height="36"
            rx="4"
            stroke="var(--line-2)"
            strokeWidth="1.3"
          />
          <text x="386" y={iy(i) + 23} fontSize="11.5" fill="var(--muted)">
            /rad/data/…/
            <tspan fill="var(--green)">pk{i + 1}</tspan>
          </text>
        </g>
      ))}
      <text x="480" y="40" textAnchor="middle" fontSize="12.5" fill="var(--green-mid)">
        N Gets
      </text>
    </Fig>
  );
}

// ── nested include traversal: recursive, per-row relationship fetches ───────
export function IncludeTraversalDiagram() {
  function Node({
    x,
    y,
    w,
    label,
    sub,
    tone,
  }: {
    x: number;
    y: number;
    w: number;
    label: string;
    sub?: string;
    tone?: "root" | "null";
  }) {
    const nul = tone === "null";
    const root = tone === "root";
    return (
      <g>
        <rect
          x={x}
          y={y}
          width={w}
          height={sub ? 46 : 34}
          rx="5"
          stroke={root ? "var(--green)" : nul ? "var(--line)" : "var(--line-2)"}
          strokeWidth={root ? 1.7 : 1.4}
          strokeDasharray={nul ? "4 4" : undefined}
          fill={root ? "var(--green-deep)" : "none"}
        />
        <text
          x={x + w / 2}
          y={y + (sub ? 21 : 22)}
          textAnchor="middle"
          fontSize="12.5"
          fontWeight={root ? 700 : 500}
          fill={root ? "var(--green)" : nul ? "var(--faint)" : "var(--ink)"}
        >
          {label}
        </text>
        {sub && (
          <text
            x={x + w / 2}
            y={y + 38}
            textAnchor="middle"
            fontSize="10.5"
            fill={nul ? "var(--faint)" : "var(--green-mid)"}
          >
            {sub}
          </text>
        )}
      </g>
    );
  }
  const tasks = [0, 1, 2];
  const ty = (i: number) => 34 + i * 104;
  return (
    <Fig
      viewBox="0 0 660 350"
      label="A nested read. The root board is fetched with one Get. Its tasks are an index scan plus a Get each. Then, per task, the assignee parent is one Get (or nothing when the FK is NULL) and the comments children are another index scan plus a Get each — a recursive, per-row traversal."
      caption="Includes are recursive per-row fetches, not joins. Every parent row independently drives its relationship reads; a NULL parent FK costs no KV op at all."
    >
      <Node x={20} y={148} w={120} label="board b1" sub="Get" tone="root" />
      <Arrow x1={140} y1={165} x2={196} y2={70} />
      <text x={150} y={120} fontSize="10.5" fill="var(--green-mid)">
        Scan+Get
      </text>
      {tasks.map((i) => (
        <g key={i}>
          {i > 0 && <Arrow x1={140} y1={171} x2={196} y2={ty(i) + 17} />}
          <Node
            x={196}
            y={ty(i)}
            w={120}
            label={`task t${i + 1}`}
            sub="Get"
          />
          {/* assignee (parent) */}
          <Arrow x1={316} y1={ty(i) + 8} x2={392} y2={ty(i) - 8} dash={i === 2} />
          {i === 2 ? (
            <Node
              x={392}
              y={ty(i) - 26}
              w={230}
              label="assignee = null · NULL FK, no KV op"
              tone="null"
            />
          ) : (
            <Node
              x={392}
              y={ty(i) - 26}
              w={150}
              label="assignee"
              sub="Get (parent)"
            />
          )}
          {/* comments (children) */}
          <Arrow x1={316} y1={ty(i) + 26} x2={392} y2={ty(i) + 44} />
          <Node
            x={392}
            y={ty(i) + 30}
            w={190}
            label="comments[]"
            sub="Scan + Get per row"
          />
        </g>
      ))}
    </Fig>
  );
}
