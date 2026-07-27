import {
  IRDiagram,
  StackDiagram,
  ToolchainDiagram,
} from "@/components/blog-diagrams";
import {
  AbstractionCurve,
  ClauseOrder,
  CompilerPipeline,
  LLVMHourglass,
  SharedIR,
} from "@/components/relational-ir-diagrams";
import { SiteNav } from "@/components/site-nav";
import { blogSource } from "@/lib/source";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";

// Blog posts are flat (content/blog/*.mdx), so the fumadocs array slug is a
// single segment — collapse it to a string for the [slug] route.
export function generateStaticParams() {
  return blogSource.generateParams().map((p) => ({ slug: (p.slug ?? []).join("/") }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>;
}): Promise<Metadata> {
  const { slug } = await params;
  const page = blogSource.getPage([slug]);
  if (!page) return {};
  return { title: `${page.data.title} — rad blog`, description: page.data.description };
}

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

function fmtDate(date?: string) {
  if (!date) return "";
  return new Date(date).toLocaleDateString("en-US", {
    year: "numeric",
    month: "long",
    day: "numeric",
  });
}

export default async function BlogPost({
  params,
}: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await params;
  const page = blogSource.getPage([slug]);
  if (!page) notFound();

  const MDX = page.data.body;
  const meta = [fmtDate(page.data.date), page.data.author]
    .filter(Boolean)
    .join(" · ");

  return (
    <div className="page">
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav />
      <main className="wrap">
        <article className="post-article">
          <Link className="post-article__back" href="/blog">
            ← blog
          </Link>
          <header className="doc__head">
            {meta && <p className="post-article__meta">{meta}</p>}
            <h1>{page.data.title}</h1>
            {page.data.description && (
              <p className="doc__desc">{page.data.description}</p>
            )}
          </header>
          <div className="doc__body" style={{ marginTop: "2.5rem" }}>
            <MDX
              components={{
                Callout,
                ToolchainDiagram,
                StackDiagram,
                IRDiagram,
                CompilerPipeline,
                LLVMHourglass,
                ClauseOrder,
                SharedIR,
                AbstractionCurve,
              }}
            />
          </div>
        </article>
      </main>
    </div>
  );
}
