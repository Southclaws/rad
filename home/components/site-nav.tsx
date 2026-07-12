import Link from "next/link";
import { GitHubMark } from "./icons";

const GITHUB = "https://github.com/Southclaws/rad";

// The shared top bar, used on the landing page and the docs.
export function SiteNav({ docsHref = "/docs" }: { docsHref?: string }) {
  return (
    <header className="nav">
      <div className="wrap nav__in">
        <Link className="brand" href="/" aria-label="Rad home">
          <b>rad</b>
          <span className="tag">v0</span>
        </Link>
        <nav className="nav__links" aria-label="Primary">
          <Link className="nav__hide" href="/#loop">
            the loop
          </Link>
          <Link className="nav__hide" href="/#build">
            what you get
          </Link>
          <Link href={docsHref}>docs</Link>
          <Link href="/blog">blog</Link>
          <a className="nav__gh" href={GITHUB} aria-label="Rad on GitHub">
            <GitHubMark />
          </a>
        </nav>
      </div>
    </header>
  );
}
