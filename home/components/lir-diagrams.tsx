// Schematic diagrams for the LIR spec, in the home page's hairline / phosphor
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

export function LIRFrontendDiagram() {
  const frontends = [
    {
      x: 20,
      label: "generated client",
      sub: "typed application queries",
    },
    {
      x: 225,
      label: "SQL frontend",
      sub: "supported SQL lowered to LIR",
    },
    {
      x: 430,
      label: "another frontend",
      sub: "its syntax, the same boundary",
    },
  ];

  return (
    <Fig
      viewBox="0 0 640 390"
      label="Three query frontends converge on LIR. Rad binds and plans the LIR relation graph, then executes the resulting plan transactionally."
      caption="Frontends own their developer-facing syntax. LIR is the common relational boundary that Rad binds, plans, and executes."
    >
      {frontends.map((frontend, index) => (
        <g key={frontend.label}>
          <rect
            x={frontend.x}
            y="18"
            width="190"
            height="66"
            rx="7"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={frontend.x + 95}
            y="44"
            textAnchor="middle"
            fontSize="13.5"
            fontWeight="600"
            fill="var(--ink)"
          >
            {frontend.label}
          </text>
          <text
            x={frontend.x + 95}
            y="65"
            textAnchor="middle"
            fontSize="10.5"
            fill="var(--faint)"
          >
            {frontend.sub}
          </text>
          <Arrow
            x1={frontend.x + 95}
            y1={84}
            x2={250 + index * 70}
            y2={142}
          />
        </g>
      ))}

      <rect
        x="220"
        y="146"
        width="200"
        height="58"
        rx="7"
        fill="var(--green-deep)"
        stroke="var(--green)"
        strokeWidth="1.8"
      />
      <text
        x="320"
        y="171"
        textAnchor="middle"
        fontSize="16"
        fontWeight="700"
        fill="var(--green)"
      >
        LIR
      </text>
      <text
        x="320"
        y="190"
        textAnchor="middle"
        fontSize="11"
        fill="var(--green-mid)"
      >
        relations · expressions · result shape
      </text>

      <Down x={320} y1={204} y2={245} />
      <rect
        x="180"
        y="249"
        width="280"
        height="54"
        rx="7"
        stroke="var(--line-2)"
        strokeWidth="1.4"
      />
      <text
        x="320"
        y="272"
        textAnchor="middle"
        fontSize="14"
        fontWeight="600"
        fill="var(--ink)"
      >
        binder and query planner
      </text>
      <text
        x="320"
        y="290"
        textAnchor="middle"
        fontSize="10.5"
        fill="var(--faint)"
      >
        catalog names · types · access paths
      </text>

      <Down x={320} y1={303} y2={340} />
      <rect
        x="180"
        y="344"
        width="280"
        height="38"
        rx="7"
        stroke="var(--green-mid)"
        strokeWidth="1.4"
      />
      <text
        x="320"
        y="368"
        textAnchor="middle"
        fontSize="13.5"
        fontWeight="600"
        fill="var(--ink)"
      >
        transactional execution
      </text>
    </Fig>
  );
}

// ── the read pipeline: wire LIR down to KV and back up as JSON ──────────────
export function LIRPipeline() {
  const stages: {
    label: string;
    sub: string;
    tone?: "hero" | "kv";
  }[] = [
    { label: "generated client", sub: "assembles a query graph" },
    {
      label: "POST /execute",
      sub: "the rad:// wire · {nodes, root}",
      tone: "hero",
    },
    { label: "binder", sub: "names → slots · types · cardinality" },
    { label: "physical planner", sub: "access paths · ordering · attaches" },
    {
      label: "executor",
      sub: "pull operators · batched correlation · folds",
    },
    {
      label: "01_kv over SlateDB",
      sub: "Get · Scan[start, end) · Put · Delete",
      tone: "kv",
    },
    { label: "result tree", sub: "nested records, shaped like your data" },
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
      label="A vertical pipeline: the generated client assembles a query graph, which travels over POST /execute, through the binder and physical planner, into the executor, down to the KV store over SlateDB, and back up as nested JSON."
      caption="One read, top to bottom: the graph is assembled at the client, bound, planned, executed against the KV store, and reassembled as nested JSON."
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
    k === "enc"
      ? "var(--green-mid)"
      : k === "id"
      ? "var(--line-2)"
      : "var(--line)";
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
      title: "index range scan",
      when: "eq prefix + range bound",
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
      caption="Constraint extraction reduces filters to per-column domains: equalities and range bounds. Precedence: complete PK → longest equality prefix (+ trailing range, + ordering tie-break) → full scan. The full filter is still re-checked on every fetched row."
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
        access selection
      </text>
      <text
        x="320"
        y="60"
        textAnchor="middle"
        fontSize="11.5"
        fill="var(--green-mid)"
      >
        per-column domains: eq + ranges
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
            {"{indexed_tuple}"} · <tspan fill="var(--green)">pk{i + 1}</tspan>
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
      <text
        x="480"
        y="40"
        textAnchor="middle"
        fontSize="12.5"
        fill="var(--green-mid)"
      >
        N Gets
      </text>
    </Fig>
  );
}

// ── correlated attaches: deduplicated batched fetches, not per-row loops ────
export function IncludeTraversalDiagram() {
  const boards = [
    { id: "b1", y: 30, owner: "ada" },
    { id: "b2", y: 96, owner: "ada" },
    { id: "b3", y: 162, owner: "bob" },
  ];
  return (
    <Fig
      viewBox="0 0 660 300"
      label="Three board rows flow into one batch. Their owner keys deduplicate to two distinct users, fetched with one point get each; their board ids drive one index scan per distinct board for tasks. No fetch happens per row."
      caption="Correlated relations execute per DISTINCT key over the batch, not per row: ada owns two boards but is fetched once, and a NULL key costs no KV work at all. Grandchildren batch across each inner batch in turn."
    >
      {boards.map((b) => (
        <g key={b.id}>
          <rect
            x="20"
            y={b.y}
            width="120"
            height="40"
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x="80"
            y={b.y + 19}
            textAnchor="middle"
            fontSize="12.5"
            fontWeight={500}
            fill="var(--ink)"
          >
            board {b.id}
          </text>
          <text
            x="80"
            y={b.y + 33}
            textAnchor="middle"
            fontSize="10.5"
            fill="var(--faint)"
          >
            owner_id = {b.owner}
          </text>
          <Arrow x1={140} y1={b.y + 20} x2={216} y2={116} />
        </g>
      ))}

      <rect
        x="216"
        y="66"
        width="150"
        height="100"
        rx="7"
        stroke="var(--green)"
        strokeWidth="1.7"
        fill="var(--green-deep)"
      />
      <text
        x="291"
        y="106"
        textAnchor="middle"
        fontSize="13.5"
        fontWeight={700}
        fill="var(--green)"
      >
        dedupe keys
      </text>
      <text
        x="291"
        y="126"
        textAnchor="middle"
        fontSize="11"
        fill="var(--green-mid)"
      >
        over the batch
      </text>

      <Arrow x1={366} y1={96} x2={452} y2={52} />
      <Arrow x1={366} y1={116} x2={452} y2={116} />
      <Arrow x1={366} y1={136} x2={452} y2={228} />
      <text x="382" y="72" fontSize="10.5" fill="var(--green-mid)">
        owners: 2 distinct
      </text>
      <text x="382" y="205" fontSize="10.5" fill="var(--green-mid)">
        tasks: 3 distinct
      </text>

      <rect
        x="452"
        y="30"
        width="186"
        height="40"
        rx="5"
        stroke="var(--line-2)"
        strokeWidth="1.4"
      />
      <text x="545" y="49" textAnchor="middle" fontSize="12" fill="var(--ink)">
        PKGet users[ada]
      </text>
      <text
        x="545"
        y="62"
        textAnchor="middle"
        fontSize="10"
        fill="var(--faint)"
      >
        shared by b1 and b2
      </text>
      <rect
        x="452"
        y="96"
        width="186"
        height="40"
        rx="5"
        stroke="var(--line-2)"
        strokeWidth="1.4"
      />
      <text x="545" y="120" textAnchor="middle" fontSize="12" fill="var(--ink)">
        PKGet users[bob]
      </text>
      <rect
        x="452"
        y="208"
        width="186"
        height="60"
        rx="5"
        stroke="var(--line-2)"
        strokeWidth="1.4"
      />
      <text x="545" y="232" textAnchor="middle" fontSize="12" fill="var(--ink)">
        IndexRangeScan tasks
      </text>
      <text
        x="545"
        y="250"
        textAnchor="middle"
        fontSize="10.5"
        fill="var(--faint)"
      >
        one per distinct board id
      </text>
    </Fig>
  );
}
