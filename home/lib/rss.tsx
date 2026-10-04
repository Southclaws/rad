import { blogDiagrams, Callout } from "@/components/blog-components";
import { renderHtml } from "@/lib/render-html";
import { blogSource } from "@/lib/source";
import type { ComponentProps } from "react";

const siteUrl = "https://www.radengine.dev";
const feedUrl = `${siteUrl}/rss.xml`;

function escapeXml(value: string): string {
  return value.replace(/[<>&"']/g, (char) => {
    return {
      "<": "&lt;",
      ">": "&gt;",
      "&": "&amp;",
      '"': "&quot;",
      "'": "&apos;",
    }[char]!;
  });
}

export async function buildRss(): Promise<string> {
  const diagrams = Object.fromEntries(
    await Promise.all(
      Object.entries(blogDiagrams).map(async ([name, Diagram]) => {
        const html = await renderHtml(<Diagram />);
        const label = html.match(/<svg\b[^>]*\baria-label="([^"]*)"/)?.[1];
        if (!label) throw new Error(`Blog diagram ${name} needs a description`);
        const image = `<img src="${siteUrl}/blog/diagrams/${name}.png" alt="${label}" style="max-width:100%;height:auto" />`;
        const figure = html.replace(/<svg\b[\s\S]*?<\/svg>/, image);
        return [
          name,
          () => <div dangerouslySetInnerHTML={{ __html: figure }} />,
        ];
      }),
    ),
  );

  const posts = blogSource
    .getPages()
    .sort((a, b) => (b.data.date ?? "").localeCompare(a.data.date ?? ""));
  const items = await Promise.all(
    posts.map(async (post) => {
      const url = new URL(post.url, siteUrl).href;
      const MDX = post.data.body;
      const html = await renderHtml(
        <MDX
          components={{
            ...diagrams,
            Callout,
            a: ({ href, ...props }: ComponentProps<"a">) => (
              <a {...props} href={href ? new URL(href, url).href : undefined} />
            ),
            img: ({ src, ...props }: ComponentProps<"img">) => (
              <img
                {...props}
                src={typeof src === "string" ? new URL(src, url).href : src}
              />
            ),
          }}
        />,
      );
      const date = post.data.date ? new Date(post.data.date) : undefined;
      if (date && Number.isNaN(date.getTime())) {
        throw new Error(`Invalid publication date for ${post.url}`);
      }
      return `<item>
<title>${escapeXml(post.data.title)}</title>
<link>${escapeXml(url)}</link>
<guid isPermaLink="true">${escapeXml(url)}</guid>
${date ? `<pubDate>${date.toUTCString()}</pubDate>` : ""}
${post.data.author ? `<dc:creator>${escapeXml(post.data.author)}</dc:creator>` : ""}
<description>${escapeXml(html)}</description>
</item>`;
    }),
  );

  return `<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:dc="http://purl.org/dc/elements/1.1/">
<channel>
<title>Rad blog</title>
<link>${siteUrl}/blog</link>
<description>Notes from building Rad: design and internals.</description>
<language>en</language>
<atom:link href="${feedUrl}" rel="self" type="application/rss+xml" />
${items.join("\n")}
</channel>
</rss>`;
}
