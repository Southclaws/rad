# Design

Visual system for the Rad brand site (`./home`). Register: brand.
Aesthetic lane: **terminal-native** — the page reads as a developer's own
environment. Committed dark treatment; the green carries the brand.

Mood, in one phrase: _a phosphor-green terminal glowing in a dark room at 2am
— clean, precise, quietly hacker-romantic._ The surface IS part of the brand
(a CRT screen), so the near-black carries a whisper of green rather than being
pure neutral.

## Theme

Single committed **dark** theme. Not dark "because tools look cool dark" —
dark because the product's habitat is a terminal on a dark editor, and a
green-on-ink CRT is the literal metaphor. No light-mode toggle on the landing
page; a light mode would dilute the one idea. (Fumadocs docs pages may theme
separately; the landing page commits.)

Color strategy: **Committed** — one saturated green spread across the surface
carries the identity; amber is the single deliberate second color.

## Color (OKLCH)

```css
/* surface — green-whisper black, the CRT glass */
--bg: oklch(0.16 0.008 160); /* page */
--surface: oklch(0.205 0.01 162); /* terminal windows, panels */
--surface-2: oklch(0.245 0.012 162); /* raised / inset within panels */
--line: oklch(0.3 0.012 160); /* hairline borders */
--line-2: oklch(0.4 0.015 160); /* stronger borders, focus rings base */

/* ink — near-white with a faint cool-green cast */
--ink: oklch(0.94 0.008 150); /* body text  (~15:1 on --bg) */
--muted: oklch(0.74 0.012 150); /* secondary  (~7.5:1 on --bg) */
--faint: oklch(0.58 0.013 150); /* tertiary / code comments (large/deco only) */

/* green — phosphor primary */
--green: oklch(0.85 0.17 150); /* bright glow: CTA, key accents */
--green-mid: oklch(0.72 0.16 150); /* links, borders, prompts (the seed) */
--green-deep: oklch(0.3 0.06 155); /* tint fills behind text */
--on-green: oklch(0.16 0.008 160); /* text on a green fill = --bg */

/* amber — the second phosphor (dual-CRT wink); used sparingly */
--amber: oklch(0.66 0.16 58); /* highlights, the "evolve" step, warnings */
--amber-mid: oklch(0.8 0.13 72); /* amber on dark text contexts */
--on-amber: oklch(0.98 0.006 150); /* white text on amber fill */

/* red — only inside code demos (conflict/error output) */
--red: oklch(0.66 0.19 25);
```

Contrast: `--ink` and `--muted` clear AA on `--bg`. Green primary L 0.85 ≤
0.18 chroma (below the fluorescent-zone cap). Green↔amber lightness gap keeps
them distinct (ratio ≥1.7). Text on a green fill is `--on-green` (dark, the
classic terminal button); text on an amber fill is white (`--on-amber`) per
the Helmholtz-Kohlrausch rule. State is never color-alone — green/amber
signals always carry a glyph or label.

## Typography

Two families on a real contrast axis (monospace × humanist sans). Mono is
earned — this is a literal CLI and database, not costume — so it leads.

- **Spline Sans Mono** (400/500/700) — _primary voice._ Wordmark, hero
  display, all headings, terminal chrome, code, nav, buttons, labels, data.
  Chosen over the IBM Plex / JetBrains / Space Mono reflexes: clean, modern,
  mechanical without being cute.
- **Hanken Grotesk** (400/500/600) — _prose counterweight._ Body paragraphs
  and longer descriptive copy, where continuous mono would tire the eye. A
  humanist grotesque, not Inter/DM.

Scale: fluid `clamp()`, ratio ≥1.25. Hero display max ≤ 6rem. Display
letter-spacing ≥ -0.03em (mono runs a touch loose; tighten headings toward
-0.02em, never past -0.03em). `text-wrap: balance` on h1–h3; `pretty` on
prose. Body line length capped 68ch. Line-height +0.06 on light-on-dark.

## Spacing & Layout

- Content column max ~1120px; prose measure ~68ch. Hero and full-bleed
  terminal panels may exceed the column.
- Fluid section rhythm with `clamp()` — generous vertical separation between
  movements, tight grouping within a terminal window. Vary it; no uniform
  stack.
- Flexbox for 1D rows, Grid only for genuine 2D. Breakpoint-free card grids
  (where cards are truly right): `repeat(auto-fit, minmax(280px, 1fr))`.
- Semantic z-index scale: `--z-nav: 100; --z-sticky: 200; --z-overlay: 300;
--z-tooltip: 400`.

### Signature components

- **Terminal window** — titlebar with a `rad` label and three dots rendered
  as monospace glyphs (not skeuomorphic), a prompt line, and output. The
  primary hero imagery. Real `rad.schema.yaml`, real generated client, real CLI
  output — never lorem.
- **Prompt line** — `$` in `--green-mid`, command in `--ink`, a block cursor
  `▌` that blinks (reduced-motion: solid).
- **The loop** — the define→run→generate→write→evolve→regenerate sequence is
  a _real ordered flow_, so numbering it is earned information, not `01/02/03`
  scaffolding. Rendered as a stepped path, not identical cards.
- **Buttons** — primary is a `--green` fill with `--on-green` text and a soft
  phosphor glow on hover; secondary is a `--line-2` outline that lights to
  `--green-mid`.

Bans honored: no side-stripe borders, no gradient text, no decorative glass,
no hero-metric template, no identical card grids, no per-section uppercase
eyebrows, no `01/02/03` except the one earned sequence.

## Motion

Intentional, part of the build — but restrained (dry, not loud).

- **Hero boot** — on load, the terminal "boots": the prompt types `rad
serve`, then output lines print in a short stagger, cursor blinks. One
  orchestrated moment, not fade-on-scroll everywhere.
- **Section reveals** — subtle, tailored per section (the loop staggers its
  steps; a code panel crossfades). Reveals enhance an already-visible default
  — content is never gated behind a transition that could fail headless.
- Easing: ease-out-expo / quart. No bounce, no elastic.
- **`prefers-reduced-motion: reduce`** — every animation degrades to the
  final static state or a crossfade; the boot shows completed output instantly
  and the cursor stops blinking.

## Accessibility

WCAG 2.1 AA. Visible focus rings (`--green-mid`, ≥2px, offset). Decorative
terminal glyphs `aria-hidden`. Semantic landmarks, honest headings, keyboard
paths for anything interactive. Alt/label text carries the dry voice.
