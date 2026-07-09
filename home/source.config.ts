import { defineDocs, defineConfig, frontmatterSchema } from "fumadocs-mdx/config";
import { z } from "zod";

// The content source: MDX under content/docs. We use fumadocs-core + this
// content layer headless — no fumadocs-ui — and style the docs with plain CSS
// to match the landing page's schematic-terminal system.
export const docs = defineDocs({
  dir: "content/docs",
});

// The blog: same MDX pipeline, plus a date and author on each post.
export const blog = defineDocs({
  dir: "content/blog",
  docs: {
    schema: frontmatterSchema.extend({
      date: z.string().optional(),
      author: z.string().optional(),
    }),
  },
});

export default defineConfig({
  mdxOptions: {
    // Force a single dark syntax theme so highlighted code reads correctly
    // without fumadocs-ui's theme classes driving the light/dark switch.
    rehypeCodeOptions: {
      themes: { light: "github-dark", dark: "github-dark" },
    },
  },
});
