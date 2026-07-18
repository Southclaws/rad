# Product

## Register

brand

## Users

Backend and full-stack developers evaluating how they talk to a database.
They've written enough SQL and fought enough ORM impedance-mismatch to be
skeptical of any tool that promises to fix it. Context: skimming a landing
page on a laptop, mid-evaluation, with a terminal already open in the next
window. The job to be done in the first 30 seconds: _understand what Rad is,
decide whether it's real, and find out how to try it._ They respond to
verifiable claims and actual code, not adjectives.

## Product Purpose

Rad is an ORM-native relational database — Go, built on SlateDB — where the
product is the developer experience, not the storage engine. The whole pitch
is one loop:

> define a schema → run the database → generate a typed client → write your
> app → evolve the schema → regenerate the client → **never write SQL.**

The landing page exists to make that loop legible and credible in one scroll,
and to route the right people to install, docs, and the GitHub repo. Success
is a skeptical engineer thinking "wait, that's actually the workflow I want"
and running the install command. Rad is honest about being an early POC; the
confidence is in the _idea and the ergonomics_, not in production maturity.

## Brand Personality

Precise, engineered, dry-witted. Speaks to developers as peers — no
onboarding hand-holding, no growth-team superlatives. Claims are specific
enough to check. Personality comes through the terminal-native treatment and
the occasional dry wink (the `7237` port has a reason that is never, ever
explained), not through exclamation marks or hype. Three words: **precise,
engineered, dry.** Emotional goal: the quiet recognition of a tool built by
someone who has the same complaints you do.

## Anti-references

- **Generic SaaS landing.** Gradient hero, a floating tilted dashboard
  screenshot, three identical icon-heading-text feature cards, a "trusted by"
  logo wall, cartoon-blob illustrations.
- **Database-blue / enterprise.** Navy-and-cyan, corporate stock photography,
  the Oracle/Mongo/enterprise-DB visual clichés. Rad is green-on-ink on
  purpose, to dodge the entire category reflex.
- **AI-slop tells.** Tiny uppercase tracked eyebrows above every section,
  `01 / 02 / 03` numbered markers as scaffolding, gradient text, decorative
  glassmorphism.

## Design Principles

1. **Show the actual thing.** Real `rad.schema.yaml`, the real generated client,
   real `rad` CLI output. The DX _is_ the pitch, so demonstrate it — never
   substitute a metaphor or an abstract diagram for the concrete artifact.
2. **Speak to peers.** Precise, checkable claims over persuasion. If a line
   couldn't survive a skeptical engineer reading it, cut it.
3. **The terminal is home.** The developer's environment is Rad's native
   habitat; the page should look like it belongs on the same screen as the
   editor and the shell.
4. **Dry wit, never loud.** Personality through restraint, precision, and one
   or two deadpan winks — not volume. Honest about POC scope, confident about
   the idea.
5. **Earn the mono.** Monospace and terminal chrome are literal here, not
   decorative — so commit fully, while keeping prose genuinely readable.

## Accessibility & Inclusion

Target WCAG 2.1 AA. Body text ≥4.5:1 and large text ≥3:1 against the ink
background (green-on-ink is naturally high-contrast; muted secondary text is
the one to verify). State is never color-alone — green/amber signals carry a
label or glyph. Every animation has a `prefers-reduced-motion: reduce`
alternative (crossfade or instant). Fully keyboard-navigable with visible
focus; the terminal/code motifs are decorative and are hidden from assistive
tech or given honest text alternatives.
