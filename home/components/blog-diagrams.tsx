// Schematic diagrams for the blog, in the home page's hairline / phosphor
// style. Provided to MDX via the components map, used as <ToolchainDiagram />
// etc. inside posts.

// Scattered, roll-your-own tools → one integrated toolchain.
export function ToolchainDiagram() {
  const scattered = [
    { t: "compiler", x: 12, y: 18 },
    { t: "formatter", x: 120, y: 52 },
    { t: "tests", x: 20, y: 104 },
    { t: "deps", x: 132, y: 112 },
  ];
  const bundled = ["compiler", "formatter", "tests", "deps"];
  return (
    <figure className="fig">
      <svg
        viewBox="0 0 600 176"
        fill="none"
        role="img"
        aria-label="On the left, four scattered tools: compiler, formatter, tests, deps. An arrow points to the right, where the same four sit inside a single box labelled 'one toolchain'."
      >
        {scattered.map((b) => (
          <g key={b.t}>
            <rect
              x={b.x}
              y={b.y}
              width="92"
              height="40"
              rx="4"
              stroke="var(--line-2)"
              strokeWidth="1.4"
            />
            <text
              x={b.x + 46}
              y={b.y + 25}
              fill="var(--muted)"
              fontSize="13"
              textAnchor="middle"
            >
              {b.t}
            </text>
          </g>
        ))}
        <g stroke="var(--green-mid)" strokeWidth="1.7" strokeLinecap="round">
          <line x1="250" y1="88" x2="306" y2="88" />
          <line x1="306" y1="88" x2="296" y2="81" />
          <line x1="306" y1="88" x2="296" y2="95" />
        </g>
        <rect
          x="332"
          y="16"
          width="267"
          height="144"
          rx="8"
          stroke="var(--green-mid)"
          strokeWidth="1.6"
        />
        <text
          x="460"
          y="40"
          fill="var(--green)"
          fontSize="13"
          textAnchor="middle"
        >
          one toolchain
        </text>
        {bundled.map((t, i) => {
          const x = 348 + (i % 2) * 124;
          const y = 56 + Math.floor(i / 2) * 48;
          return (
            <g key={t}>
              <rect
                x={x}
                y={y}
                width="112"
                height="36"
                rx="4"
                stroke="var(--line-2)"
                strokeWidth="1.3"
              />
              <text
                x={x + 56}
                y={y + 23}
                fill="var(--muted)"
                fontSize="12.5"
                textAnchor="middle"
              >
                {t}
              </text>
            </g>
          );
        })}
      </svg>
      <figcaption>
        One vision driving the whole stack: the thing Go got right.
      </figcaption>
    </figure>
  );
}

// OLTP welds every layer behind one door (SQL); OLAP decomposes them.
export function StackDiagram() {
  const layers = ["protocol", "query lang", "planner", "storage"];
  return (
    <figure className="fig">
      <svg
        viewBox="0 0 600 288"
        fill="none"
        role="img"
        aria-label="Left, an OLTP database: protocol, query language, planner and storage welded into one block, reachable only through a SQL door. Right, an OLAP stack: several engines over a shared metadata layer and shared object storage."
      >
        <text
          x="178"
          y="26"
          fill="var(--faint)"
          fontSize="13"
          textAnchor="middle"
        >
          OLTP · welded
        </text>
        <rect
          x="105"
          y="44"
          width="150"
          height="220"
          rx="6"
          stroke="var(--line-2)"
          strokeWidth="1.6"
        />
        {layers.map((l, i) => (
          <g key={l}>
            {i > 0 && (
              <line
                x1="105"
                y1={44 + 55 * i}
                x2="255"
                y2={44 + 55 * i}
                stroke="var(--line)"
                strokeWidth="1.2"
              />
            )}
            <text
              x="180"
              y={44 + 55 * i + 33}
              fill="var(--muted)"
              fontSize="12.5"
              textAnchor="middle"
            >
              {l}
            </text>
          </g>
        ))}
        <rect
          x="26"
          y="131"
          width="60"
          height="46"
          rx="4"
          fill="var(--green-deep)"
          stroke="var(--green-mid)"
          strokeWidth="1.5"
        />
        <text
          x="56"
          y="159"
          fill="var(--green)"
          fontSize="13"
          textAnchor="middle"
        >
          SQL
        </text>
        <line
          x1="86"
          y1="154"
          x2="105"
          y2="154"
          stroke="var(--green-mid)"
          strokeWidth="1.5"
        />

        <line
          x1="300"
          y1="40"
          x2="300"
          y2="264"
          stroke="var(--line)"
          strokeWidth="1"
          strokeDasharray="4 6"
        />

        <text
          x="450"
          y="26"
          fill="var(--faint)"
          fontSize="13"
          textAnchor="middle"
        >
          OLAP · decomposed
        </text>
        {[350, 420, 490].map((x) => (
          <g key={x}>
            <rect
              x={x}
              y="52"
              width="60"
              height="42"
              rx="4"
              stroke="var(--line-2)"
              strokeWidth="1.4"
            />
            <text
              x={x + 30}
              y={78}
              fill="var(--muted)"
              fontSize="11.5"
              textAnchor="middle"
            >
              engine
            </text>
            <line
              x1={x + 30}
              y1="94"
              x2={x + 30}
              y2="150"
              stroke="var(--green-mid)"
              strokeWidth="1.3"
            />
          </g>
        ))}
        <rect
          x="350"
          y="150"
          width="200"
          height="34"
          rx="4"
          stroke="var(--line-2)"
          strokeWidth="1.4"
        />
        <text
          x="450"
          y="172"
          fill="var(--muted)"
          fontSize="12.5"
          textAnchor="middle"
        >
          metadata · Iceberg
        </text>
        <rect
          x="350"
          y="204"
          width="200"
          height="46"
          rx="4"
          fill="var(--green-deep)"
          stroke="var(--green-mid)"
          strokeWidth="1.5"
        />
        <text
          x="450"
          y="232"
          fill="var(--green)"
          fontSize="12"
          textAnchor="middle"
        >
          object storage · Parquet
        </text>
      </svg>
      <figcaption>
        OLTP couples every layer behind one door. OLAP decomposes them: shared
        storage, competing engines.
      </figcaption>
    </figure>
  );
}

// Many front-ends converge on one IR, which targets the core below.
export function IRDiagram() {
  const ins = ["SQL", "typed client", "…"];
  const outs = [
    { t: "planner + exec", x: 110 },
    { t: "storage", x: 330 },
  ];
  return (
    <figure className="fig">
      <svg
        viewBox="0 0 560 232"
        fill="none"
        role="img"
        aria-label="Three front-ends: SQL, a typed client, and others: converge on a central query IR box, which fans out to a planner/execution box and a storage box."
      >
        {ins.map((t, i) => {
          const x = 40 + i * 180;
          return (
            <g key={t}>
              <rect
                x={x}
                y="18"
                width="120"
                height="40"
                rx="5"
                stroke="var(--line-2)"
                strokeWidth="1.4"
              />
              <text
                x={x + 60}
                y={42}
                fill="var(--muted)"
                fontSize="13"
                textAnchor="middle"
              >
                {t}
              </text>
              <line
                x1={x + 60}
                y1="58"
                x2="280"
                y2="100"
                stroke="var(--green-mid)"
                strokeWidth="1.3"
              />
            </g>
          );
        })}
        <rect
          x="212"
          y="100"
          width="136"
          height="44"
          rx="6"
          fill="var(--green-deep)"
          stroke="var(--green)"
          strokeWidth="1.7"
        />
        <text
          x="280"
          y="128"
          fill="var(--green)"
          fontSize="15"
          fontWeight="700"
          textAnchor="middle"
        >
          LIR
        </text>
        {outs.map((o) => (
          <g key={o.t}>
            <line
              x1="280"
              y1="144"
              x2={o.x + 60}
              y2="188"
              stroke="var(--green-mid)"
              strokeWidth="1.3"
            />
            <rect
              x={o.x}
              y="188"
              width="120"
              height="40"
              rx="5"
              stroke="var(--line-2)"
              strokeWidth="1.4"
            />
            <text
              x={o.x + 60}
              y={212}
              fill="var(--muted)"
              fontSize="13"
              textAnchor="middle"
            >
              {o.t}
            </text>
          </g>
        ))}
      </svg>
      <figcaption>
        SQL could be one of many ways in. An IR lets many front-ends target one
        relational core: the LLVM move, for databases.
      </figcaption>
    </figure>
  );
}
