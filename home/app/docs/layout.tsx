import { DocsSidebar } from "@/components/docs-sidebar";
import { SiteNav } from "@/components/site-nav";
import { source } from "@/lib/source";
import type { ReactNode } from "react";

type SidebarNode =
  | { type: "page"; name: string; url: string }
  | {
      type: "folder";
      name: string;
      index?: { name: string; url: string };
      children: SidebarNode[];
    };

type TreeNode = {
  type: string;
  name?: ReactNode;
  url?: string;
  index?: { name?: ReactNode; url: string };
  children?: TreeNode[];
};

// Keep Fumadocs' hierarchy intact rather than flattening folders into links.
// The narrow serialisable shape is all the client-side disclosure needs.
function toSidebarNodes(nodes: TreeNode[] = []): SidebarNode[] {
  return nodes.flatMap((node): SidebarNode[] => {
    if (node.type === "page" && node.url) {
      return [{ type: "page", name: String(node.name), url: node.url }];
    }

    if (node.type === "folder") {
      return [
        {
          type: "folder",
          name: String(node.name),
          ...(node.index && {
            index: { name: String(node.index.name), url: node.index.url },
          }),
          children: toSidebarNodes(node.children),
        },
      ];
    }

    return [];
  });
}

export default function DocsLayout({ children }: { children: ReactNode }) {
  const items = toSidebarNodes(
    (source.pageTree as { children?: TreeNode[] }).children,
  );

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
