import type { ReactNode } from "react";

function Fig({
  label,
  caption,
  viewBox,
  children,
}: {
  label: string;
  caption?: ReactNode;
  viewBox: string;
  children: ReactNode;
}) {
  return (
    <figure className="fig">
      <svg viewBox={viewBox} fill="none" role="img" aria-label={label}>
        {children}
      </svg>
      {caption != null && <figcaption>{caption}</figcaption>}
    </figure>
  );
}

// A straight arrow from (x1,y1) to (x2,y2) with a small head at the end.
function Arrow({
  x1,
  y1,
  x2,
  y2,
  color = "var(--green-mid)",
  width = 1.4,
  dash,
}: {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  color?: string;
  width?: number;
  dash?: boolean;
}) {
  const a = Math.atan2(y2 - y1, x2 - x1);
  const h = 7;
  return (
    <g stroke={color} strokeWidth={width} strokeLinecap="round" fill="none">
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

// A plain thin line (no head), for meshes and connectors.
function Line({
  x1,
  y1,
  x2,
  y2,
  color = "var(--green-mid)",
  width = 1.3,
  dash,
}: {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  color?: string;
  width?: number;
  dash?: boolean;
}) {
  return (
    <line
      x1={x1}
      y1={y1}
      x2={x2}
      y2={y2}
      stroke={color}
      strokeWidth={width}
      strokeLinecap="round"
      strokeDasharray={dash ? "4 6" : undefined}
    />
  );
}

// A database is a compiler that also stores things: the front of the pipeline
// is the same shape, only the last two stages diverge.
export function CompilerPipeline() {
  const cols = [
    { a: "lex", b: "lex" },
    { a: "parse", b: "parse" },
    { a: "AST", b: "AST" },
    { a: "optimise", b: "plan" },
    { a: "codegen", b: "execute" },
  ];
  const X0 = 104;
  const W = 96;
  const STEP = 118;
  const H = 46;
  const rowA = 44;
  const rowB = 122;
  const cx = (i: number) => X0 + i * STEP + W / 2;
  const dividerX = X0 + 3 * STEP - 11;

  function Row({
    y,
    keys,
    hero,
    label,
    labelColor,
  }: {
    y: number;
    keys: string[];
    hero: boolean;
    label: string;
    labelColor: string;
  }) {
    return (
      <g>
        <text x={16} y={y + H / 2 + 4} fontSize="12.5" fill={labelColor}>
          {label}
        </text>
        {keys.map((t, i) => {
          const last = i === keys.length - 1;
          const emphasise = hero && last;
          const stroke = hero ? "var(--green-mid)" : "var(--line-2)";
          const x = X0 + i * STEP;
          return (
            <g key={t}>
              {i > 0 && (
                <Arrow
                  x1={X0 + (i - 1) * STEP + W}
                  y1={y + H / 2}
                  x2={x}
                  y2={y + H / 2}
                  color={hero ? "var(--green-mid)" : "var(--line-2)"}
                  width={1.3}
                />
              )}
              <rect
                x={x}
                y={y}
                width={W}
                height={H}
                rx="6"
                stroke={emphasise ? "var(--green)" : stroke}
                strokeWidth={emphasise ? 1.8 : 1.5}
                fill={emphasise ? "var(--green-deep)" : "none"}
              />
              <text
                x={x + W / 2}
                y={y + H / 2 + 5}
                textAnchor="middle"
                fontSize="13.5"
                fontWeight={emphasise ? 700 : 500}
                fill={
                  emphasise
                    ? "var(--green)"
                    : hero
                      ? "var(--ink)"
                      : "var(--muted)"
                }
              >
                {t}
              </text>
            </g>
          );
        })}
      </g>
    );
  }

  return (
    <Fig
      viewBox="0 0 690 196"
      label="Two aligned pipelines. A compiler: lex, parse, AST, optimise, codegen. A database: lex, parse, AST, plan, execute. The first three stages are identical; a dashed divider marks where the last two diverge — codegen versus execute."
    >
      <text
        x={cx(1)}
        y={20}
        textAnchor="middle"
        fontSize="11"
        fill="var(--faint)"
      >
        same shape
      </text>
      <text
        x={cx(3) + 14}
        y={20}
        textAnchor="middle"
        fontSize="11"
        fill="var(--faint)"
      >
        diverges
      </text>
      <Line
        x1={dividerX}
        y1={26}
        x2={dividerX}
        y2={182}
        color="var(--line-2)"
        width={1}
        dash
      />
      <Row
        y={rowA}
        keys={cols.map((c) => c.a)}
        hero={false}
        label="compiler"
        labelColor="var(--faint)"
      />
      <Row
        y={rowB}
        keys={cols.map((c) => c.b)}
        hero
        label="database"
        labelColor="var(--green-mid)"
      />
    </Fig>
  );
}

// LLVM's move: many language front-ends converge on one IR, which fans back out
// to many hardware back-ends. The neat system compilers get to enjoy.
export function LLVMHourglass() {
  const fronts = ["C++", "Rust", "Swift", "Zig"];
  const backs = ["x86", "Arm", "PowerPC", "RISC-V"];
  const LW = 118;
  const RW = 118;
  const H = 40;
  const leftX = 24;
  const rightX = 498;
  const ys = [24, 84, 144, 204];
  const cyOf = (y: number) => y + H / 2;
  const irX = 262;
  const irW = 116;
  const irCy = 134;
  const irH = 64;
  const irY = irCy - irH / 2;

  return (
    <Fig
      viewBox="0 0 640 262"
      label="An hourglass. On the left, four language front-ends — C++, Rust, Swift, Zig — all converge on a central LLVM IR box. From it, lines fan back out to four hardware back-ends: x86, Arm, PowerPC, RISC-V."
      caption="LLVM's trick: define one IR in the middle. Write a front-end for it once and every back-end comes along for free."
    >
      <text x={leftX} y={14} fontSize="11.5" fill="var(--faint)">
        languages
      </text>
      <text
        x={rightX + RW}
        y={14}
        textAnchor="end"
        fontSize="11.5"
        fill="var(--faint)"
      >
        hardware targets
      </text>

      {fronts.map((t, i) => (
        <g key={t}>
          <Line x1={leftX + LW} y1={cyOf(ys[i])} x2={irX} y2={irCy} />
          <rect
            x={leftX}
            y={ys[i]}
            width={LW}
            height={H}
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={leftX + LW / 2}
            y={cyOf(ys[i]) + 5}
            textAnchor="middle"
            fontSize="13"
            fill="var(--muted)"
          >
            {t}
          </text>
        </g>
      ))}

      {backs.map((t, i) => (
        <g key={t}>
          <Line x1={irX + irW} y1={irCy} x2={rightX} y2={cyOf(ys[i])} />
          <rect
            x={rightX}
            y={ys[i]}
            width={RW}
            height={H}
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={rightX + RW / 2}
            y={cyOf(ys[i]) + 5}
            textAnchor="middle"
            fontSize="13"
            fill="var(--muted)"
          >
            {t}
          </text>
        </g>
      ))}

      <rect
        x={irX}
        y={irY}
        width={irW}
        height={irH}
        rx="8"
        stroke="var(--green)"
        strokeWidth="1.8"
        fill="var(--green-deep)"
      />
      <text
        x={irX + irW / 2}
        y={irCy - 2}
        textAnchor="middle"
        fontSize="15"
        fontWeight="700"
        fill="var(--green)"
      >
        LLVM IR
      </text>
      <text
        x={irX + irW / 2}
        y={irCy + 16}
        textAnchor="middle"
        fontSize="10.5"
        fill="var(--green-mid)"
      >
        the middle ground
      </text>
    </Fig>
  );
}

// SQL's clause order is fixed by grammar, not by how data moves: connectors
// between written order and run order cross, and SELECT makes the big jump.
export function ClauseOrder() {
  const written = ["SELECT", "FROM", "WHERE", "GROUP BY", "ORDER BY", "LIMIT"];
  const executed = ["FROM", "WHERE", "GROUP BY", "SELECT", "ORDER BY", "LIMIT"];
  const W = 96;
  const STEP = 110;
  const X0 = 16;
  const topY = 46;
  const botY = 158;
  const H = 36;
  const centerX = (slot: number) => X0 + slot * STEP + W / 2;

  function Pills({
    order,
    y,
    hero,
  }: {
    order: string[];
    y: number;
    hero: boolean;
  }) {
    return (
      <g>
        {order.map((t, i) => (
          <g key={t}>
            <rect
              x={X0 + i * STEP}
              y={y}
              width={W}
              height={H}
              rx="6"
              stroke={hero ? "var(--green-mid)" : "var(--line-2)"}
              strokeWidth="1.5"
              fill={hero ? "var(--green-deep)" : "none"}
              fillOpacity={hero ? 0.4 : 1}
            />
            <text
              x={X0 + i * STEP + W / 2}
              y={y + H / 2 + 5}
              textAnchor="middle"
              fontSize="12.5"
              fontWeight={hero ? 600 : 500}
              fill={hero ? "var(--green)" : "var(--muted)"}
            >
              {t}
            </text>
          </g>
        ))}
      </g>
    );
  }

  return (
    <Fig
      viewBox="0 0 680 232"
      label="Two rows of SQL clauses. As written: SELECT, FROM, WHERE, GROUP BY, ORDER BY, LIMIT. As run, following the data: FROM, WHERE, GROUP BY, SELECT, ORDER BY, LIMIT. Connector lines between the two rows cross; SELECT moves from first-written to fourth-run."
    >
      <text x={X0} y={30} fontSize="11.5" fill="var(--faint)">
        as written
      </text>
      <Pills order={written} y={topY} hero={false} />

      {written.map((clause) => {
        const from = centerX(written.indexOf(clause));
        const to = centerX(executed.indexOf(clause));
        const isSelect = clause === "SELECT";
        return (
          <Line
            key={clause}
            x1={from}
            y1={topY + H}
            x2={to}
            y2={botY}
            color={isSelect ? "var(--amber-mid)" : "var(--green-mid)"}
            width={isSelect ? 1.7 : 1.2}
          />
        );
      })}

      <Pills order={executed} y={botY} hero />
      <text x={X0} y={botY + H + 22} fontSize="11.5" fill="var(--green-mid)">
        logical evaluation order
      </text>
    </Fig>
  );
}

// The payoff: every syntax built into every engine is N×M; lowering them all
// into one shared relational IR collapses that to N+M.
export function SharedIR() {
  const fronts = ["pipe", "SQL", "client"];
  const enginesL = ["Postgres", "MySQL", "etc..."];
  const enginesR = ["Postgres", "MySQL", "etc..."];
  const ys = [66, 138, 210];
  const H = 34;
  const cyOf = (y: number) => y + H / 2;

  return (
    <Fig
      viewBox="0 0 700 284"
      label="Two panels. Left, labelled N times M: three query front-ends each connect directly to three separate database engines, a full mesh of nine lines. Right, labelled N plus M: the same three front-ends converge on one shared relational IR box, which fans out to three engines — far fewer connections."
    >
      {/* left panel — today */}
      <text x={20} y={40} fontSize="12" fill="var(--faint)">
        no shared IR
      </text>
      <text
        x={200}
        y={40}
        textAnchor="middle"
        fontSize="15"
        fontWeight="700"
        fill="var(--amber-mid)"
      >
        N × M
      </text>
      {fronts.map((f, i) =>
        enginesL.map((_, j) => (
          <Line
            key={`${f}-${j}`}
            x1={20 + 78}
            y1={cyOf(ys[i])}
            x2={150}
            y2={cyOf(ys[j])}
            color="var(--faint)"
            width={1}
          />
        )),
      )}
      {fronts.map((t, i) => (
        <g key={`lf-${t}`}>
          <rect
            x={20}
            y={ys[i]}
            width={78}
            height={H}
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={20 + 39}
            y={cyOf(ys[i]) + 4}
            textAnchor="middle"
            fontSize="12"
            fill="var(--muted)"
          >
            {t}
          </text>
        </g>
      ))}
      {enginesL.map((t, i) => (
        <g key={`le-${t}`}>
          <rect
            x={150}
            y={ys[i]}
            width={104}
            height={H}
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={150 + 52}
            y={cyOf(ys[i]) + 4}
            textAnchor="middle"
            fontSize="11.5"
            fill="var(--muted)"
          >
            {t}
          </text>
        </g>
      ))}

      <Line
        x1={314}
        y1={24}
        x2={314}
        y2={270}
        color="var(--line)"
        width={1}
        dash
      />

      {/* right panel — with a shared IR */}
      <text x={372} y={40} fontSize="12" fill="var(--green-mid)">
        with a shared IR
      </text>
      <text
        x={630}
        y={40}
        textAnchor="middle"
        fontSize="15"
        fontWeight="700"
        fill="var(--green)"
      >
        N + M
      </text>
      {fronts.map((_, i) => (
        <Line
          key={`rc-${i}`}
          x1={372 + 78}
          y1={cyOf(ys[i])}
          x2={486}
          y2={cyOf(ys[1])}
        />
      ))}
      {enginesR.map((_, i) => (
        <Line
          key={`rd-${i}`}
          x1={486 + 92}
          y1={cyOf(ys[1])}
          x2={598}
          y2={cyOf(ys[i])}
        />
      ))}
      {fronts.map((t, i) => (
        <g key={`rf-${t}`}>
          <rect
            x={372}
            y={ys[i]}
            width={78}
            height={H}
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={372 + 39}
            y={cyOf(ys[i]) + 4}
            textAnchor="middle"
            fontSize="12"
            fill="var(--muted)"
          >
            {t}
          </text>
        </g>
      ))}
      <rect
        x={486}
        y={cyOf(ys[1]) - 38}
        width={92}
        height={76}
        rx="8"
        stroke="var(--green)"
        strokeWidth="1.8"
        fill="var(--green-deep)"
      />
      <text
        x={486 + 46}
        y={cyOf(ys[1]) - 4}
        textAnchor="middle"
        fontSize="13"
        fontWeight="700"
        fill="var(--green)"
      >
        relational
      </text>
      <text
        x={486 + 46}
        y={cyOf(ys[1]) + 15}
        textAnchor="middle"
        fontSize="13"
        fontWeight="700"
        fill="var(--green)"
      >
        IR
      </text>
      {enginesR.map((t, i) => (
        <g key={`re-${t}`}>
          <rect
            x={598}
            y={ys[i]}
            width={86}
            height={H}
            rx="5"
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={598 + 43}
            y={cyOf(ys[i]) + 4}
            textAnchor="middle"
            fontSize="11.5"
            fill="var(--muted)"
          >
            {t}
          </text>
        </g>
      ))}
    </Fig>
  );
}

// A smooth (Catmull-Rom) path through points, so the curve reads as a loose
// sketch rather than a precise plot.
function smoothPath(pts: { x: number; y: number }[]) {
  const r = (v: number) => Math.round(v * 10) / 10;
  const d = [`M ${r(pts[0].x)} ${r(pts[0].y)}`];
  for (let i = 0; i < pts.length - 1; i++) {
    const p0 = pts[i - 1] ?? pts[i];
    const p1 = pts[i];
    const p2 = pts[i + 1];
    const p3 = pts[i + 2] ?? p2;
    const c1x = p1.x + (p2.x - p0.x) / 6;
    const c1y = p1.y + (p2.y - p0.y) / 6;
    const c2x = p2.x - (p3.x - p1.x) / 6;
    const c2y = p2.y - (p3.y - p1.y) / 6;
    d.push(`C ${r(c1x)} ${r(c1y)} ${r(c2x)} ${r(c2y)} ${r(p2.x)} ${r(p2.y)}`);
  }
  return d.join(" ");
}

// Abstraction level across the query lifecycle. An ORM raises imperative code
// up to SQL, then the engine lowers it all the way back down — a round-trip
// over the peak. Skip SQL and the path is a gradual descent.
export function AbstractionCurve() {
  const originX = 88;
  const baseY = 272;
  const topY = 44;
  const rightX = 624;
  const ticks = [
    { x: 156, label: "application code" },
    { x: 289, label: "SQL" },
    { x: 421, label: "query planner" },
    { x: 554, label: "physical" },
  ];
  const app = { x: 156, y: 150 };
  const sql = { x: 289, y: 72 };
  const planner = { x: 421, y: 206 };
  const physical = { x: 554, y: 252 };
  const stages = [app, sql, planner, physical];
  const orm = [app, sql, planner, physical];
  const rad = [app, planner, physical];

  return (
    <Fig
      viewBox="0 0 660 340"
      label="A line chart. The vertical axis is abstraction level; the horizontal axis steps through application code, SQL, query planner and physical plan. The ORM path rises from application code up to SQL — the highest point — then falls through the query planner down to the physical plan. The Rad path skips the SQL peak, descending gradually from application code straight to the physical plan."
      caption="an emotional rollercoaster."
    >
      {ticks.map((t) => (
        <Line
          key={`grid-${t.label}`}
          x1={t.x}
          y1={topY}
          x2={t.x}
          y2={baseY}
          color="var(--line)"
          width={1}
          dash
        />
      ))}

      <g stroke="var(--line-2)" strokeWidth="1.4" strokeLinecap="round">
        <line x1={originX} y1={topY} x2={originX} y2={baseY} />
        <line x1={originX} y1={baseY} x2={rightX} y2={baseY} />
        <line x1={originX} y1={topY} x2={originX - 5} y2={topY + 8} />
        <line x1={originX} y1={topY} x2={originX + 5} y2={topY + 8} />
      </g>
      <text
        transform="rotate(-90 30 158)"
        x={30}
        y={158}
        textAnchor="middle"
        fontSize="12"
        fill="var(--faint)"
      >
        abstraction level
      </text>

      {ticks.map((t) => (
        <g key={`tick-${t.label}`}>
          <line
            x1={t.x}
            y1={baseY}
            x2={t.x}
            y2={baseY + 5}
            stroke="var(--line-2)"
            strokeWidth="1.4"
          />
          <text
            x={t.x}
            y={baseY + 20}
            textAnchor="middle"
            fontSize="11"
            fill="var(--muted)"
          >
            {t.label}
          </text>
        </g>
      ))}

      <path
        d={smoothPath(orm)}
        stroke="var(--amber-mid)"
        strokeWidth="2"
        fill="none"
        strokeLinecap="round"
        strokeLinejoin="round"
      />

      {stages.map((p) => (
        <circle
          key={`${p.x}-${p.y}`}
          cx={p.x}
          cy={p.y}
          r="3.5"
          fill="var(--ink)"
        />
      ))}

      <g>
        <line
          x1={372}
          y1={64}
          x2={402}
          y2={64}
          stroke="var(--amber-mid)"
          strokeWidth="2"
          strokeLinecap="round"
        />
        <text x={410} y={68} fontSize="11.5" fill="var(--amber-mid)">
          ORM: raise to SQL, then lower
        </text>
      </g>
    </Fig>
  );
}
