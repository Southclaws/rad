"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useRef, useState } from "react";

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

function isCurrentBranch(node: SidebarNode, pathname: string): boolean {
  if (node.type === "page") return pathname === node.url;
  if (node.type === "separator") return false;
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
        ) : node.type === "separator" ? (
          <li
            className="docs__separator"
            key={`separator-${node.name ?? "rule"}`}
          >
            {node.name}
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
        {node.index ? (
          <>
            <button
              type="button"
              className="docs__foldertoggle docs__foldertoggle--icon"
              aria-expanded={open}
              aria-label={`${open ? "Collapse" : "Expand"} ${node.name}`}
              onClick={() => setOpen((value) => !value)}
            >
              <span aria-hidden="true">›</span>
            </button>
            <Link
              href={node.index.url}
              aria-current={pathname === node.index.url ? "page" : undefined}
            >
              {node.index.name}
            </Link>
          </>
        ) : (
          <button
            type="button"
            className="docs__foldertoggle"
            aria-expanded={open}
            onClick={() => setOpen((value) => !value)}
          >
            <span aria-hidden="true">›</span>
            <strong>{node.name}</strong>
          </button>
        )}
      </div>
      {open && <Tree nodes={node.children} pathname={pathname} />}
    </li>
  );
}

export function DocsSidebar({
  sections,
  locale,
}: {
  sections: SidebarSection[];
  locale: "en" | "fr";
}) {
  const pathname = usePathname();
  const switcher = useRef<HTMLDetailsElement>(null);
  const current =
    sections.find((section) =>
      section.items.some((item) => isCurrentBranch(item, pathname)),
    ) ?? sections[0];

  useEffect(() => {
    if (switcher.current) switcher.current.open = false;
  }, [pathname]);

  useEffect(() => {
    function close(event: PointerEvent) {
      if (switcher.current && !switcher.current.contains(event.target as Node)) {
        switcher.current.open = false;
      }
    }
    function escape(event: KeyboardEvent) {
      if (event.key === "Escape" && switcher.current?.open) {
        switcher.current.open = false;
        switcher.current.querySelector("summary")?.focus();
      }
    }
    document.addEventListener("pointerdown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("pointerdown", close);
      document.removeEventListener("keydown", escape);
    };
  }, []);

  return (
    <nav className="docs__nav" aria-label="Documentation">
      <details className="docs__section" ref={switcher}>
        <summary>
          <span>
            <small>Section</small>
            <strong>{current.name}</strong>
          </span>
          <span className="docs__sectionchevron" aria-hidden="true">⌄</span>
        </summary>
        <div className="docs__sectionmenu">
          {sections.map((section) => {
            const selected = section === current;
            return (
              <Link
                key={section.href}
                href={section.href}
                aria-current={selected ? "true" : undefined}
              >
                <span>
                  <strong>{section.name}</strong>
                  {section.description && <small>{section.description}</small>}
                </span>
                {selected && <span aria-hidden="true">✓</span>}
              </Link>
            );
          })}
        </div>
      </details>
      {current.description && (
        <p className="docs__sectiondesc">{current.description}</p>
      )}
      <Tree nodes={current.items} pathname={pathname} />
      <div className="docs__languages" aria-label="Documentation language">
        <Link href="/docs" aria-current={locale === "en" ? "true" : undefined}>
          English
        </Link>
        <Link href="/fr/docs" aria-current={locale === "fr" ? "true" : undefined}>
          Français
        </Link>
      </div>
    </nav>
  );
}
