import { source } from "@/lib/source";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";

export function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}): Promise<Metadata> {
  const { slug } = await params;
  const page = source.getPage(slug);
  if (!page) return {};
  return {
    title: `${page.data.title} — Rad docs`,
    description: page.data.description,
  };
}

// Callout is the one custom MDX component our content uses. Full border with a
// tint — no side-stripe.
function Callout({ children }: { children: ReactNode }) {
  return (
    <div className="callout" role="note">
      <span className="callout__tag" aria-hidden="true">
        note
      </span>
      <div>{children}</div>
    </div>
  );
}

const mdxComponents = { Callout };

export default async function Page({
  params,
}: {
  params: Promise<{ slug?: string[] }>;
}) {
  const { slug } = await params;
  const page = source.getPage(slug);
  if (!page) notFound();

  const MDX = page.data.body;
  const toc = page.data.toc ?? [];

  return (
    <article className="doc">
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
          <aside className="doc__toc" aria-label="On this page">
            <p className="doc__toclabel">On this page</p>
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
        <Link href="/">← back to rad</Link>
      </footer>
    </article>
  );
}
