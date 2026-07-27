import { ArrowUpRight } from "@/components/icons";
import { NotFoundTerminal } from "@/components/not-found-terminal";
import { SiteNav } from "@/components/site-nav";
import type { Metadata } from "next";

export const metadata: Metadata = {
  title: "404 — not found · rad",
};

export default function NotFound() {
  return (
    <div className="page">
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav />
      <main>
        <section className="hero nf">
          <div className="wrap">
            <p className="slabel">error · 404</p>
            <h1 className="nf__code" aria-label="404">
              404
            </h1>
            <p className="nf__title">
              This page returned <span className="mark">zero rows.</span>
            </p>
            <NotFoundTerminal />
            <p className="nf__joke">
              {"// "}a SQL query walks into a bar, walks up to two tables, and
              asks: &ldquo;can I join you?&rdquo;
            </p>
            <div className="hero__cta nf__cta">
              <a className="btn btn--primary" href="/">
                Back to rad
              </a>
              <a className="btn btn--ghost" href="/docs">
                Read the docs <ArrowUpRight />
              </a>
            </div>
          </div>
        </section>
      </main>
    </div>
  );
}
