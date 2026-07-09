import { loader } from "fumadocs-core/source";
import { blog, docs } from "@/.source/server";

// Fumadocs content loaders. baseUrl mounts docs at /docs and the blog at
// /blog; page lookups feed our plain-CSS layouts.
export const source = loader({
  baseUrl: "/docs",
  source: docs.toFumadocsSource(),
});

export const blogSource = loader({
  baseUrl: "/blog",
  source: blog.toFumadocsSource(),
});
