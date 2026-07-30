// GitHub social-preview card generator.
//
//   node scripts/gen-social.mjs
//
// The schematic-terminal system as a still card, mirroring the site's
// opengraph image at GitHub's recommended social-preview size (1280×640):
// phosphor green on ink, the extruded wordmark, the hero line, and a
// prompt/URL footer under a hairline. Satori renders text to vector paths;
// sharp rasterizes at 2× for retina. Output:
//   public/rad-social.png    2560×1280 — upload via repo settings →
//                            Social preview
import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import satori from "satori";
import sharp from "sharp";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const p = (...s) => join(root, ...s);

const WIDTH = 1280;
const HEIGHT = 640;
const SCALE = 2;

// Palette (DESIGN.md tokens as hex — Satori parses hex most reliably).
const BG = "#0d1411";
const INK = "#e9f1eb";
const MUTED = "#a7b4ac";
const GREEN = "#5cf29b";
const GREEN_MID = "#45cf82";
const GREEN_DEEP = "#123a26";
const LINE = "#333e39";

const WORDMARK = "rad";
const HEADLINE = "Radically rethink your relationship with rows.";
const SUB =
  "The relational database, redesigned. With one cohesive toolchain. From schema to application.";
const PROMPT = "rad serve";
const URLTAG = "radengine.dev";

// Satori needs TrueType/OpenType (not woff2). Google's css2 endpoint serves
// truetype to a plain fetch (no woff2-capable UA), subset to `text`.
async function loadFont(weight, text) {
  const url = `https://fonts.googleapis.com/css2?family=Spline+Sans+Mono:wght@${weight}&text=${encodeURIComponent(
    text,
  )}`;
  const css = await (await fetch(url)).text();
  const src = css.match(/src: url\((.+?)\) format\('(?:opentype|truetype)'\)/);
  if (!src) throw new Error(`could not resolve Spline Sans Mono @${weight}`);
  return Buffer.from(await (await fetch(src[1])).arrayBuffer());
}

const text = WORDMARK + HEADLINE + SUB + PROMPT + URLTAG + "$";
const fonts = await Promise.all(
  [400, 500, 700].map(async (weight) => ({
    name: "Spline Sans Mono",
    data: await loadFont(weight, text),
    weight,
    style: "normal",
  })),
);

// Satori VDOM helpers (no JSX in a plain .mjs).
const div = (style, children) => ({ type: "div", props: { style, children } });
const span = (style, children) => ({
  type: "span",
  props: { style, children },
});

const card = div(
  {
    height: "100%",
    width: "100%",
    display: "flex",
    flexDirection: "column",
    justifyContent: "space-between",
    background: BG,
    padding: 76,
    fontFamily: "Spline Sans Mono",
    position: "relative",
  },
  [
    // faint blueprint frame
    div({
      position: "absolute",
      top: 38,
      left: 38,
      right: 38,
      bottom: 38,
      border: `1px solid ${LINE}`,
    }),

    // extruded wordmark
    div({ display: "flex", alignItems: "flex-end" }, [
      div(
        {
          fontSize: 176,
          fontWeight: 700,
          lineHeight: 1,
          letterSpacing: -8,
          color: GREEN,
          textShadow: `6px 6px 0 ${GREEN_DEEP}, 12px 12px 0 ${GREEN_DEEP}`,
        },
        WORDMARK,
      ),
    ]),

    // headline + sub
    div({ display: "flex", flexDirection: "column" }, [
      div(
        {
          fontSize: 54,
          fontWeight: 700,
          letterSpacing: -1.5,
          color: INK,
          textWrap: "balance",
        },
        HEADLINE,
      ),
      div({ marginTop: 16, fontSize: 31, fontWeight: 400, color: MUTED }, SUB),
    ]),

    // footer: prompt · url, under a hairline
    div({ display: "flex", flexDirection: "column" }, [
      div({ height: 1, background: LINE, marginBottom: 22 }),
      div(
        {
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          fontSize: 28,
          fontWeight: 500,
        },
        [
          div({ display: "flex", color: INK }, [
            span({ color: GREEN_MID, marginRight: 12 }, "$"),
            span({}, PROMPT),
          ]),
          div({ color: GREEN_MID }, URLTAG),
        ],
      ),
    ]),
  ],
);

const svg = await satori(card, { width: WIDTH, height: HEIGHT, fonts });

mkdirSync(p("public"), { recursive: true });

// density scales librsvg's raster grid so the 2× resize stays vector-sharp.
await sharp(Buffer.from(svg), { density: 72 * SCALE })
  .resize(WIDTH * SCALE, HEIGHT * SCALE)
  .png()
  .toFile(p("public/rad-social.png"));

console.log(
  `social card generated: public/rad-social.png (${WIDTH * SCALE}×${HEIGHT * SCALE})`,
);
