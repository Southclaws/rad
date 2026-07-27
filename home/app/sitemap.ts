import type { MetadataRoute } from "next";
import { blogSource, source } from "@/lib/source";

const BASE = "https://www.radengine.dev";

export default function sitemap(): MetadataRoute.Sitemap {
  const docs = source.getPages().map((page) => ({
    url: `${BASE}${page.url}`,
    changeFrequency: "weekly" as const,
    priority: 0.6,
  }));
  const posts = blogSource.getPages().map((page) => ({
    url: `${BASE}${page.url}`,
    changeFrequency: "monthly" as const,
    priority: 0.5,
  }));

  return [
    { url: BASE, changeFrequency: "weekly", priority: 1 },
    { url: `${BASE}/blog`, changeFrequency: "weekly", priority: 0.7 },
    ...docs,
    ...posts,
  ];
}
