"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useRef, useState } from "react";

type SidebarNode =
  | { type: "page"; name: string; url: string }
  | {
      type: "folder";
      name: string;
      index?: { name: string; url: string };
      children: SidebarNode[];
    };

function isCurrentBranch(node: SidebarNode, pathname: string): boolean {
  if (node.type === "page") return pathname === node.url;
  return (
    pathname === node.index?.url ||
    node.children.some((child) => isCurrentBranch(child, pathname))
  );
}

function Tree({ nodes, pathname }: { nodes: SidebarNode[]; pathname: string }) {
  return (
    <ul className="docs__tree">
      {nodes.map((node) =>
        node.type === "page" ? (
          <li key={node.url}>
            <Link
              href={node.url}
              aria-current={pathname === node.url ? "page" : undefined}
            >
              {node.name}
            </Link>
          </li>
        ) : (
          <Folder key={node.index?.url ?? node.name} node={node} pathname={pathname} />
        ),
      )}
    </ul>
  );
}

function Folder({ node, pathname }: { node: Extract<SidebarNode, { type: "folder" }>; pathname: string }) {
  const [open, setOpen] = useState(() => isCurrentBranch(node, pathname));
  const previousPathname = useRef(pathname);
  const active = isCurrentBranch(node, pathname);

  // Navigating directly to a nested page opens its branch, while still
  // allowing readers to collapse the currently active folder afterwards.
  useEffect(() => {
    if (pathname !== previousPathname.current) {
      previousPathname.current = pathname;
      if (active) setOpen(true);
    }
  }, [active, pathname]);

  return (
    <li className="docs__folder" data-active={active || undefined}>
      <div className="docs__folderhead">
        <button
          type="button"
          className="docs__foldertoggle"
          aria-expanded={open}
          aria-label={`${open ? "Collapse" : "Expand"} ${node.name}`}
          onClick={() => setOpen((value) => !value)}
        >
          <span aria-hidden="true">›</span>
        </button>
        {node.index ? (
          <Link
            href={node.index.url}
            aria-current={pathname === node.index.url ? "page" : undefined}
          >
            {node.index.name}
          </Link>
        ) : (
          <span>{node.name}</span>
        )}
      </div>
      {open && <Tree nodes={node.children} pathname={pathname} />}
    </li>
  );
}

export function DocsSidebar({
  items,
}: {
  items: SidebarNode[];
}) {
  const pathname = usePathname();
  return (
    <nav className="docs__nav" aria-label="Documentation">
      <p className="docs__navlabel">Documentation</p>
      <Tree nodes={items} pathname={pathname} />
    </nav>
  );
}
