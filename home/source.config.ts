import { defineDocs, defineConfig } from "fumadocs-mdx/config";

// The content source: MDX under content/docs. We use fumadocs-core + this
// content layer headless — no fumadocs-ui — and style the docs with plain CSS
// to match the landing page's schematic-terminal system.
export const docs = defineDocs({
  dir: "content/docs",
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
