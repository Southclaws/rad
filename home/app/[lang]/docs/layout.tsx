import { DocsSidebar } from "@/components/docs-sidebar";
import { SiteNav } from "@/components/site-nav";
import { i18n, isDocsLocale } from "@/lib/i18n";
import { source } from "@/lib/source";
import { notFound } from "next/navigation";
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

export default async function DocsLayout({
  children,
  params,
}: {
  children: ReactNode;
  params: Promise<{ lang: string }>;
}) {
  const { lang } = await params;
  if (!isDocsLocale(lang)) notFound();
  const items = toSidebarNodes(
    (source.getPageTree(lang) as { children?: TreeNode[] }).children,
  );
  const docsHref = lang === i18n.defaultLanguage ? "/docs" : `/${lang}/docs`;

  return (
    <div className="page" lang={lang}>
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav docsHref={docsHref} />
      <div className="wrap docs">
        <aside className="docs__side">
          <DocsSidebar items={items} locale={lang} />
        </aside>
        <main className="docs__main">{children}</main>
      </div>
    </div>
  );
}
