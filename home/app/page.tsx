import { Artifacts } from "@/components/artifacts";
import { Backronym } from "@/components/backronym";
import { BootTerminal } from "@/components/boot-terminal";
import { CopyCommand } from "@/components/copy-command";
import { GitHubMark, ArrowUpRight } from "@/components/icons";
import { RelationGlyph } from "@/components/relation-glyph";
import { Reveal } from "@/components/reveal";
import { SiteNav } from "@/components/site-nav";

const GITHUB = "https://github.com/Southclaws/rad";
const INSTALL = "curl -fsSL https://radengine.dev/install.sh | sh";

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
    desc: "Describe your data model.",
  },
  {
    n: "02",
    verb: "migrate",
    hint: "automatic",
    desc: "The database itself handles it.",
  },
  {
    n: "03",
    verb: "generate",
    hint: "rad generate",
    desc: "Generate type-safe clients.",
  },
  {
    n: "04",
    verb: "build",
    hint: "your application",
    desc: "Write business logic, not database glue.",
  },
  {
    n: "05",
    verb: "deploy",
    hint: "object store native",
    desc: "Backed by durable object storage.",
  },
  {
    n: "06",
    verb: "repeat",
    hint: "stay in flow",
    desc: "Keep your momentum focused.",
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
                  <span className="mark">With one cohesive toolchain.</span>{" "}
                  <span className="dim">
                    True OLTP backed by durable object storage.
                  </span>
                </p>
                <CopyCommand command={INSTALL} />
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

        {/* ── the loop ─────────────────────────────────────────────────── */}
        <section className="section" id="loop">
          <div className="wrap">
            <Reveal>
              <p className="slabel">the loop</p>
              <h2 className="stitle">
                Everything starts with the <span className="mark">schema.</span>
              </h2>
              <p className="prose" style={{ marginTop: "1.25rem" }}>
                Rad's <span className="mark">declarative</span> schema is the
                source of truth for your database, migrations and type-safe
                clients. Change it, and everything else follows.
              </p>
            </Reveal>
            <Reveal className="loop" delay={80}>
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
            </Reveal>
          </div>
        </section>

        {/* ── show, don't tell ─────────────────────────────────────────── */}
        <section className="section" id="show">
          <div className="wrap">
            <Reveal>
              <p className="slabel">show, don&apos;t tell</p>
              <h2 className="stitle">Here&apos;s the whole thing.</h2>
              <p className="prose" style={{ marginTop: "1.25rem" }}>
                You author a single declarative schema. Rad generates the exact
                clients for your database. No ORMs. No query builders. No
                string-gluing.
              </p>
            </Reveal>
            <Reveal delay={80} className="">
              <div style={{ marginTop: "2.5rem" }}>
                <Artifacts />
              </div>
            </Reveal>
          </div>
        </section>

        {/* ── what you get ─────────────────────────────────────────────── */}
        <section className="section" id="build">
          <div className="wrap">
            <Reveal>
              <p className="slabel">what you get</p>
              <h2 className="stitle">
                Radically rethink your relationship with rows.
              </h2>
              <p className="prose" style={{ marginTop: "1.25rem" }}>
                The whole stack, from code to columns. Built for today, not
                1973.
              </p>
            </Reveal>
            <Reveal delay={60} className="feat">
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
            </Reveal>
          </div>
        </section>

        {/* ── get started ──────────────────────────────────────────────── */}
        <section className="section">
          <div className="wrap">
            <Reveal className="cta">
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
              <CopyCommand command={INSTALL} />
              <div className="hero__cta" style={{ justifyContent: "center" }}>
                <a className="btn btn--primary" href="/docs">
                  Documentation <ArrowUpRight />
                </a>
                <a className="btn btn--ghost" href={GITHUB}>
                  <GitHubMark size={16} /> Source code
                </a>
              </div>
            </Reveal>
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
            <a href="#loop">The loop</a>
            <a href="#build">What you get</a>
            <a href={GITHUB}>GitHub</a>
          </div>
          <div className="foot__note">
            <span className="foot__port">
              rad <b>v0</b> — a proof of concept, and honest about it.
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
