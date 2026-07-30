import { DocsSidebar } from "@/components/docs-sidebar";
import { SiteNav } from "@/components/site-nav";
import { i18n, isDocsLocale } from "@/lib/i18n";
import { source } from "@/lib/source";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";

type SidebarNode =
  | { type: "page"; name: string; url: string }
  | { type: "separator"; name?: string }
  | {
      type: "folder";
      name: string;
      index?: { name: string; url: string };
      children: SidebarNode[];
    };

type SidebarSection = {
  name: string;
  description?: string;
  href: string;
  items: SidebarNode[];
};

type TreeNode = {
  type: string;
  name?: ReactNode;
  description?: ReactNode;
  url?: string;
  root?: boolean;
  index?: { name?: ReactNode; url: string };
  children?: TreeNode[];
};

function toSidebarNodes(nodes: TreeNode[] = []): SidebarNode[] {
  return nodes.flatMap((node): SidebarNode[] => {
    if (node.type === "page" && node.url) {
      return [{ type: "page", name: String(node.name), url: node.url }];
    }

    if (node.type === "separator") {
      return [
        {
          type: "separator",
          ...(node.name && { name: String(node.name) }),
        },
      ];
    }

    if (node.type === "folder" && !node.root) {
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

function firstPage(nodes: TreeNode[] = []): string | undefined {
  for (const node of nodes) {
    if (node.type === "page" && node.url) return node.url;
    if (node.index?.url) return node.index.url;
    const nested = firstPage(node.children);
    if (nested) return nested;
  }
}

function rootSections(nodes: TreeNode[] = []): SidebarSection[] {
  return nodes.flatMap((node): SidebarSection[] => {
    if (node.type !== "folder") return [];

    const nested = rootSections(node.children);
    if (!node.root) return nested;

    const href = node.index?.url ?? firstPage(node.children);
    if (!href) return nested;
    return [
      {
        name: String(node.name),
        ...(node.description && { description: String(node.description) }),
        href,
        items: toSidebarNodes(node.children),
      },
      ...nested,
    ];
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
  const docsHref = lang === i18n.defaultLanguage ? "/docs" : `/${lang}/docs`;
  const tree = source.getPageTree(lang) as {
    name?: ReactNode;
    description?: ReactNode;
    children?: TreeNode[];
  };
  const sections: SidebarSection[] = [
    {
      name: String(tree.name ?? "Introduction"),
      ...(tree.description && { description: String(tree.description) }),
      href: docsHref,
      items: toSidebarNodes(tree.children),
    },
    ...rootSections(tree.children),
  ];

  return (
    <div className="page" lang={lang}>
      <div className="page__ticks" aria-hidden="true" />
      <SiteNav docsHref={docsHref} />
      <div className="wrap docs">
        <aside className="docs__side">
          <DocsSidebar sections={sections} locale={lang} />
        </aside>
        <main className="docs__main">{children}</main>
      </div>
    </div>
  );
}
