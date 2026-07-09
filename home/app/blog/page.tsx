import { SiteNav } from "@/components/site-nav";
import { blogSource } from "@/lib/source";
import type { Metadata } from "next";
import Link from "next/link";

export const metadata: Metadata = {
  title: "Blog — rad",
  description:
    "Notes from building Rad — design, internals, and the road ahead.",
};

function fmtDate(date?: string) {
  if (!date) return "";
  return new Date(date).toLocaleDateString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}

export default function BlogIndex() {
  const posts = blogSource.getPages().sort((a, b) => {
    const da = a.data.date ?? "";
    const db = b.data.date ?? "";
    return db.localeCompare(da); // newest first
  });

  return (
    <div className="page">
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav />
      <main className="wrap">
        <section className="section">
          <p className="slabel">blog</p>
          <h1 className="stitle">Notes from the build.</h1>

          {posts.length === 0 ? (
            <p className="prose" style={{ marginTop: "1.5rem" }}>
              Nothing here yet. Check back soon.
            </p>
          ) : (
            <ul className="postlist">
              {posts.map((post) => (
                <li key={post.url}>
                  <Link className="post" href={post.url}>
                    <span className="post__date">
                      {fmtDate(post.data.date)}
                    </span>
                    <span className="post__body">
                      <span className="post__title">{post.data.title}</span>
                      {post.data.description && (
                        <span className="post__desc">
                          {post.data.description}
                        </span>
                      )}
                    </span>
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </section>
      </main>
    </div>
  );
}
