import { DocsSidebar } from "@/components/docs-sidebar";
import { SiteNav } from "@/components/site-nav";
import { source } from "@/lib/source";
import type { ReactNode } from "react";

// Flatten the fumadocs page tree into an ordered link list for the sidebar
// (the docs are a flat set of pages; folders unwrap in order).
type Item = { name: string; url: string };
type TreeNode = {
  type: string;
  name?: ReactNode;
  url?: string;
  index?: { name?: ReactNode; url: string };
  children?: TreeNode[];
};

function flatten(nodes: TreeNode[] = []): Item[] {
  const out: Item[] = [];
  for (const n of nodes) {
    if (n.type === "page" && n.url) out.push({ name: String(n.name), url: n.url });
    else if (n.type === "folder") {
      if (n.index) out.push({ name: String(n.index.name), url: n.index.url });
      out.push(...flatten(n.children));
    }
  }
  return out;
}

export default function DocsLayout({ children }: { children: ReactNode }) {
  const items = flatten((source.pageTree as { children?: TreeNode[] }).children);

  return (
    <div className="page">
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav />
      <div className="wrap docs">
        <aside className="docs__side">
          <DocsSidebar items={items} />
        </aside>
        <main className="docs__main">{children}</main>
      </div>
    </div>
  );
}
