"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

export function DocsSidebar({
  items,
}: {
  items: { name: string; url: string }[];
}) {
  const pathname = usePathname();
  return (
    <nav className="docs__nav" aria-label="Documentation">
      <p className="docs__navlabel">Documentation</p>
      <ul>
        {items.map((it) => (
          <li key={it.url}>
            <Link
              href={it.url}
              aria-current={pathname === it.url ? "page" : undefined}
            >
              {it.name}
            </Link>
          </li>
        ))}
      </ul>
    </nav>
  );
}
