function Arrow({
  x1,
  y1,
  x2,
  y2,
}: {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
}) {
  const angle = Math.atan2(y2 - y1, x2 - x1);
  const head = 7;

  return (
    <g
      stroke="var(--green-mid)"
      strokeWidth="1.5"
      strokeLinecap="round"
      fill="none"
    >
      <line x1={x1} y1={y1} x2={x2} y2={y2} />
      <line
        x1={x2}
        y1={y2}
        x2={x2 - head * Math.cos(angle - 0.5)}
        y2={y2 - head * Math.sin(angle - 0.5)}
      />
      <line
        x1={x2}
        y1={y2}
        x2={x2 - head * Math.cos(angle + 0.5)}
        y2={y2 - head * Math.sin(angle + 0.5)}
      />
    </g>
  );
}

export function RadOverviewDiagram() {
  const frontends = [
    { label: "generated", x: 24 },
    { label: "SQL", x: 180 },
    { label: "ORM", x: 336 },
  ];

  return (
    <figure className="overview-diagram">
      <svg
        viewBox="0 0 480 480"
        fill="none"
        role="img"
        aria-label="Generated clients, SQL, and ORMs compile into Rad's LIR and PIR. The stateless relational query engine reads and writes durable key-value state in S3-compatible object storage."
      >
        <text x="24" y="22" fontSize="13" fill="var(--faint)">
          frontends
        </text>

        {frontends.map((frontend) => (
          <g key={frontend.label}>
            <rect
              x={frontend.x}
              y="38"
              width="120"
              height="46"
              rx="5"
              stroke="var(--line-2)"
              strokeWidth="1.4"
            />
            <text
              x={frontend.x + 60}
              y="66"
              textAnchor="middle"
              fontSize="15"
              fill="var(--muted)"
            >
              {frontend.label}
            </text>
          </g>
        ))}

        <Arrow x1={84} y1={84} x2={174} y2={144} />
        <Arrow x1={240} y1={84} x2={240} y2={144} />
        <Arrow x1={396} y1={84} x2={306} y2={144} />

        <rect
          x="52"
          y="112"
          width="376"
          height="236"
          rx="9"
          stroke="var(--line-2)"
          strokeWidth="1.2"
          strokeDasharray="5 6"
        />
        <text x="70" y="135" fontSize="12.5" fill="var(--faint)">
          rad
        </text>

        <rect
          x="106"
          y="148"
          width="268"
          height="62"
          rx="7"
          fill="var(--green-deep)"
          stroke="var(--green)"
          strokeWidth="1.7"
        />
        <text
          x="240"
          y="176"
          textAnchor="middle"
          fontSize="18"
          fontWeight="700"
          fill="var(--green)"
        >
          relational IR
        </text>
        <text
          x="240"
          y="195"
          textAnchor="middle"
          fontSize="12.5"
          fill="var(--green-mid)"
        >
          structured relational programs
        </text>

        <Arrow x1={240} y1={210} x2={240} y2={246} />

        <rect
          x="106"
          y="250"
          width="268"
          height="70"
          rx="7"
          stroke="var(--line-2)"
          strokeWidth="1.4"
        />
        <text
          x="240"
          y="280"
          textAnchor="middle"
          fontSize="16"
          fontWeight="600"
          fill="var(--ink)"
        >
          relational query engine
        </text>
        <text
          x="240"
          y="301"
          textAnchor="middle"
          fontSize="12.5"
          fill="var(--faint)"
        >
          bind · plan · execute
        </text>

        <Arrow x1={240} y1={320} x2={240} y2={382} />

        <rect
          x="82"
          y="386"
          width="316"
          height="70"
          rx="7"
          fill="var(--surface-2)"
          stroke="var(--green-mid)"
          strokeWidth="1.5"
        />
        <text
          x="240"
          y="416"
          textAnchor="middle"
          fontSize="15.5"
          fontWeight="600"
          fill="var(--ink)"
        >
          S3-compatible object storage
        </text>
        <text
          x="240"
          y="438"
          textAnchor="middle"
          fontSize="12.5"
          fill="var(--green-mid)"
        >
          durable key-value state
        </text>
      </svg>
    </figure>
  );
}
