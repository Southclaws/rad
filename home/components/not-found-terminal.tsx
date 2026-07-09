"use client";

import { usePathname } from "next/navigation";
import { WindowControls } from "@/components/icons";

// The 404 rendered as a real Rad error: a GET that misses, answered with an
// RFC 7807 problem document — the same shape the server speaks on the wire.
export function NotFoundTerminal() {
  const path = usePathname() || "/";

  return (
    <div
      className="term crop nf__term"
      role="img"
      aria-label={`404 not found — no page matches ${path}`}
    >
      <div className="term__bar" aria-hidden="true">
        <span className="term__title">rad — get</span>
        <WindowControls />
      </div>
      <div className="term__body" aria-hidden="true">
        <div className="term__line term__cmd">
          rad get rad://radengine.dev{path}
        </div>
        <div className="term__line term__err">
          404 · urn:rad:problem:not_found
        </div>
        <div className="term__line term__out">
          detail no page matches that path
        </div>
      </div>
    </div>
  );
}
