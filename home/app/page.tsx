import { Artifacts } from "@/components/artifacts";
import { Backronym } from "@/components/backronym";
import { BootTerminal } from "@/components/boot-terminal";
import { InstallCommand } from "@/components/copy-command";
import { GitHubMark, ArrowUpRight } from "@/components/icons";
import { RadOverviewDiagram } from "@/components/rad-overview-diagram";
import { RelationGlyph } from "@/components/relation-glyph";
import { SiteNav } from "@/components/site-nav";

const GITHUB = "https://github.com/Southclaws/rad";

const LOOP: {
  n: string;
  verb: string;
  hint: string;
  desc: string;
  last?: boolean;
}[] = [
  {
    n: "01",
    verb: "define",
    hint: "rad.schema.yaml",
    desc: "Describe tables, relations and indexes.",
  },
  {
    n: "02",
    verb: "migrate",
    hint: "rad schema migrate",
    desc: "Review and apply the schema diff.",
  },
  {
    n: "03",
    verb: "generate",
    hint: "rad generate",
    desc: "Generate schema-aware clients.",
  },
  {
    n: "04",
    verb: "build",
    hint: "your application",
    desc: "Use typed queries in application code.",
  },
  {
    n: "05",
    verb: "deploy",
    hint: "stateless compute",
    desc: "Run against durable object storage.",
  },
  {
    n: "06",
    verb: "evolve",
    hint: "the next revision",
    desc: "Change the schema and regenerate.",
    last: true,
  },
];

type Feature = {
  tag: string;
  h: string;
  p: string;
  code?: React.ReactNode;
  span2?: boolean;
};

const FEATURES: Feature[] = [
  {
    tag: "clients",
    h: "No need for an ORM",
    p: "Generated directly from your schema. Typed queries, typed rows and typed relationships. More than just a layer on top, Rad defines and enforces a contract between your data and your code.",
    span2: true,
    code: (
      <>
        db.Tasks.Query().<b>StatusEq</b>(&quot;todo&quot;).
        <b>OrderByPriority</b>().All(ctx)
      </>
    ),
  },
  {
    tag: "reads",
    h: "Joins that make sense",
    p: "Read data the way your application uses it. Parents, children and nested relationships without rebuilding from flattened results.",
  },
  {
    tag: "migrations",
    h: "Migrations from a schema diff",
    p: "Change the schema. Rad computes and runs the migration. Stable IDs preserve rename intent without unnecessary rewrites.",
    code: (
      <>
        <b>rad schema migrate</b>
      </>
    ),
  },
  {
    tag: "one binary",
    h: "Server, devtool & codegen, all in one",
    p: "Database, toolchain and code generation in a single executable. Runs anywhere you need it to.",
  },
  {
    tag: "storage",
    h: "Durable, stateless storage",
    p: "Built for object storage from day one. Stateless compute, durable data.",
  },
  {
    tag: "interface",
    h: "Strings are not an API",
    p: "Rad's lower level intermediary format is structured so you can build queries without gluing strings.",
    code: (
      <>
        "SELECT * FROM users " + <b>whereClause</b> + " ORDER BY " + ... 🙅
      </>
    ),
  },
  {
    tag: "protocol",
    h: "The rad:// protocol",
    p: "A simple JSON-over-HTTP protocol. Designed for generated clients, not hand-written requests.",
  },
];

export default function Home() {
  return (
    <div className="page">
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav />

      <main>
        {/* ── hero ─────────────────────────────────────────────────────── */}
        <section className="hero">
          <div className="wrap">
            <h1 className="wordmark">rad</h1>
            <div className="hero__lede">
              <div className="hero__aside">
                <p className="hero__thesis">
                  The relational database, redesigned.{" "}
                  <span className="dim">
                    Serverless OLTP backed by durable object storage.
                  </span>{" "}
                  <span className="mark">With a relational IR layer.</span>
                </p>
                <InstallCommand />
                <div className="hero__cta">
                  <a className="btn btn--primary" href="/docs">
                    Read the docs <ArrowUpRight />
                  </a>
                  <a className="btn btn--ghost" href={GITHUB}>
                    <GitHubMark size={16} /> Star on GitHub
                  </a>
                </div>
              </div>
              <BootTerminal />
            </div>
          </div>
        </section>

        <section className="section" id="overview">
          <div className="wrap">
            <div className="overview">
              <div className="overview__copy">
                <p className="slabel">What is Rad?</p>
                <p className="prose" style={{ marginTop: "1.25rem" }}>
                  Rad is a new <strong>relational database</strong>, unlike any
                  other.
                </p>
                <p className="prose" style={{ marginTop: "1.25rem" }}>
                  Firstly, it challenges the status-quo of SQL being the lingua
                  franca of <em>relational protocols</em>. Instead, Rad exposes
                  a <strong>relational intermediate representation</strong> as
                  its interface. This IR can be targeted by SQL dialects, ORMs,
                  or any other kind of frontend.
                </p>
                <p className="prose" style={{ marginTop: "1.25rem" }}>
                  Secondly, it's backed by <strong>object storage</strong>{" "}
                  rather than disks. This sacrifices speed for infrastructural
                  simplicity. This means Rad can run in serverless environments
                  with just an S3-compatible bucket and a stateless compute
                  environment. You can think of Rad as a small relational query
                  engine over a key-value store.
                </p>
              </div>
              <div className="overview__visual">
                <RadOverviewDiagram />
              </div>
            </div>
          </div>
        </section>

        {/* ── intermediate representation ─────────────────────────────── */}
        <section className="section" id="ir">
          <div className="wrap">
            <div>
              <p className="slabel">LIR + PIR</p>
              <h2 className="stitle">A relational database IR.</h2>
              <p className="prose" style={{ marginTop: "1.25rem" }}>
                Rad&apos;s generated clients compile transactions into
                structured programs. Inspired by LLVM, the shared intermediate
                layer separates frontend syntax from structured machine-friendly
                commands.
              </p>
            </div>
            <div>
              <div className="ir-example">
                <Artifacts />
              </div>
            </div>
          </div>
        </section>

        {/* ── schema-driven workflow ───────────────────────────────────── */}
        <section className="section" id="workflow">
          <div className="wrap">
            <div>
              <p className="slabel">development workflow</p>
              <h2 className="stitle">Schema driven</h2>
              <p className="prose" style={{ marginTop: "1.25rem" }}>
                Rad&apos;s <span className="mark">declarative</span> schema is
                the source of truth for your database, migrations and type-safe
                clients. Change it, and everything else follows.
              </p>
            </div>
            <div className="loop">
              {LOOP.map((s) => (
                <div className="node" key={s.n}>
                  <div className="node__n">
                    <span className="node__dot" aria-hidden="true" />
                    {s.n}
                  </div>
                  <div className="node__cmd">
                    <span className={s.last ? "node__last" : undefined}>
                      {s.verb}
                    </span>
                  </div>
                  <div className="node__hint">{s.hint}</div>
                  <p className="node__desc">{s.desc}</p>
                </div>
              ))}
            </div>
          </div>
        </section>

        {/* ── what you get ─────────────────────────────────────────────── */}
        <section className="section" id="build">
          <div className="wrap">
            <div>
              <p className="slabel">what you get</p>
              <h2 className="stitle">
                Radically rethink your relationship with rows.
              </h2>
              <p className="prose" style={{ marginTop: "1.25rem" }}>
                The whole stack, from code to columns. Built for today, not
                1973.
              </p>
            </div>
            <div className="feat">
              {FEATURES.map((f) => (
                <div
                  className={`cell${f.span2 ? " cell--span2" : ""}`}
                  key={f.h}
                >
                  <span className="cell__tag">{f.tag}</span>
                  <h3 className="cell__h">{f.h}</h3>
                  <p className="cell__p">{f.p}</p>
                  {f.code && <code className="cell__code">{f.code}</code>}
                </div>
              ))}
              <div className="cell cell--deco">
                <RelationGlyph />
              </div>
            </div>
          </div>
        </section>

        {/* ── get started ──────────────────────────────────────────────── */}
        <section className="section">
          <div className="wrap">
            <div className="cta">
              <p
                className="prose"
                style={{
                  textAlign: "center",
                  marginInline: "auto",
                  textWrap: "balance",
                }}
              >
                Grab the binary for Windows, macOS or Linux. This includes the
                database server itself and the CLI developer tools.
              </p>
              <InstallCommand />
              <div className="hero__cta" style={{ justifyContent: "center" }}>
                <a className="btn btn--primary" href="/docs">
                  Documentation <ArrowUpRight />
                </a>
                <a className="btn btn--ghost" href={GITHUB}>
                  <GitHubMark size={16} /> Source code
                </a>
              </div>
            </div>
          </div>
        </section>
      </main>

      <footer className="foot">
        <div className="wrap">
          <Backronym />
        </div>
        <div className="wrap foot__in">
          <div className="foot__cols">
            <a href="/docs">Docs</a>
            <a href={GITHUB}>GitHub</a>
          </div>
          <div className="foot__note">
            <span className="foot__port">
              rad <b>v0</b> - a proof of concept. for now...
            </span>
          </div>
        </div>
        <div className="wrap foot__type">
          Set in <a href="/licenses/SplineSansMono-OFL.txt">Spline Sans Mono</a>{" "}
          &amp; <a href="/licenses/HankenGrotesk-OFL.txt">Hanken Grotesk</a> —
          SIL OFL 1.1.
        </div>
      </footer>
    </div>
  );
}
