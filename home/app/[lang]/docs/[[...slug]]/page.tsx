import { source } from "@/lib/source";
import { isDocsLocale } from "@/lib/i18n";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import {
  LIRPipeline,
  KeyLayoutDiagram,
  AccessPathDiagram,
  IndexScanDiagram,
  IncludeTraversalDiagram,
} from "@/components/lir-diagrams";

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ lang: string; slug?: string[] }>;
}): Promise<Metadata> {
  const { lang, slug } = await params;
  if (!isDocsLocale(lang)) return {};
  const page = source.getPage(slug, lang);
  if (!page) return {};
  return {
    title: `${page.data.title} — Rad docs`,
    description: page.data.description,
  };
}

function Callout({
  type = "note",
  children,
}: {
  type?: "note" | "warn";
  children: ReactNode;
}) {
  return (
    <div className={`callout callout--${type}`} role="note">
      <span className="callout__tag" aria-hidden="true">
        {type}
      </span>
      <div>{children}</div>
    </div>
  );
}

const mdxComponents = {
  Callout,
  LIRPipeline,
  KeyLayoutDiagram,
  AccessPathDiagram,
  IndexScanDiagram,
  IncludeTraversalDiagram,
};

export default async function Page({
  params,
}: {
  params: Promise<{ lang: string; slug?: string[] }>;
}) {
  const { lang, slug } = await params;
  if (!isDocsLocale(lang)) notFound();
  const page = source.getPage(slug, lang);
  if (!page) notFound();

  const MDX = page.data.body;
  const toc = page.data.toc ?? [];
  const isFrench = lang === "fr";

  return (
    <article className="doc" lang={lang}>
      <header className="doc__head">
        <h1>{page.data.title}</h1>
        {page.data.description && (
          <p className="doc__desc">{page.data.description}</p>
        )}
      </header>
      <div className="doc__grid">
        <div className="doc__body">
          <MDX components={mdxComponents} />
        </div>
        {toc.length > 0 && (
          <aside className="doc__toc" aria-label={isFrench ? "Sur cette page" : "On this page"}>
            <p className="doc__toclabel">{isFrench ? "Sur cette page" : "On this page"}</p>
            <ul>
              {toc.map((item) => (
                <li key={item.url} data-depth={item.depth}>
                  <a href={item.url}>{item.title}</a>
                </li>
              ))}
            </ul>
          </aside>
        )}
      </div>
      <footer className="doc__foot">
        <Link href="/">{isFrench ? "← retour à rad" : "← back to rad"}</Link>
      </footer>
    </article>
  );
}
