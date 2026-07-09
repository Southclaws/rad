// Brand + icon generator.
//
//   node scripts/gen-brand.mjs
//
// Renders the real Spline Sans Mono letterforms with Satori (text → vector
// paths, so the output needs no font to display), then rasterizes with sharp
// and bundles a favicon.ico. Outputs:
//   public/brand/rad-wordmark.svg | .png   the "rad" wordmark, 1:1
//   public/brand/rad-mark.svg              the "r"-in-a-square logo (source)
//   public/icons/icon-<n>.png              32 → 512 icon set
//   public/icons/apple-touch-icon.png      180
//   app/icon.svg                           served favicon (mark)
//   app/apple-icon.png                     Apple touch icon (auto-linked)
//   app/favicon.ico                        legacy favicon (16/32/48)
import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import satori from "satori";
import sharp from "sharp";
import pngToIco from "png-to-ico";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const p = (...s) => join(root, ...s);

const INK = "#0d1411";
const GREEN = "#5cf29b";

async function loadFont() {
  const url =
    "https://fonts.googleapis.com/css2?family=Spline+Sans+Mono:wght@700&text=" +
    encodeURIComponent("rad");
  const css = await (await fetch(url)).text();
  const m = css.match(/src: url\((.+?)\) format\('(?:opentype|truetype)'\)/);
  if (!m) throw new Error("could not resolve Spline Sans Mono @700");
  return Buffer.from(await (await fetch(m[1])).arrayBuffer());
}

const fonts = [
  {
    name: "Spline Sans Mono",
    data: await loadFont(),
    weight: 700,
    style: "normal",
  },
];

// Satori VDOM helpers (no JSX in a plain .mjs).
const tile = (children, extra = {}) => ({
  type: "div",
  props: {
    style: {
      width: "100%",
      height: "100%",
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      background: INK,
      fontFamily: "Spline Sans Mono",
      ...extra,
    },
    children,
  },
});
const glyph = (text, style) => ({
  type: "div",
  props: {
    style: { fontWeight: 700, color: GREEN, lineHeight: 1, ...style },
    children: text,
  },
});

const wordmarkSvg = await satori(
  tile(glyph("rad", { fontSize: 208, letterSpacing: -8 })),
  { width: 512, height: 512, fonts },
);
// The mark: a lowercase "r" (the brand is lowercase) in a rounded square.
const markSvg = await satori(
  tile(glyph("r", { fontSize: 360, marginTop: -12 }), { borderRadius: 112 }),
  { width: 512, height: 512, fonts },
);

mkdirSync(p("public/brand"), { recursive: true });
mkdirSync(p("public/icons"), { recursive: true });

writeFileSync(p("public/brand/rad-wordmark.svg"), wordmarkSvg);
writeFileSync(p("public/brand/rad-mark.svg"), markSvg);
writeFileSync(p("app/icon.svg"), markSvg);

await sharp(Buffer.from(wordmarkSvg))
  .resize(1024, 1024)
  .png()
  .toFile(p("public/brand/rad-wordmark.png"));

const png = (svg, size) =>
  sharp(Buffer.from(svg)).resize(size, size).png().toBuffer();

const SIZES = [16, 32, 48, 64, 128, 192, 256, 512];
for (const size of SIZES) {
  writeFileSync(p(`public/icons/icon-${size}.png`), await png(markSvg, size));
}
const apple = await png(markSvg, 180);
writeFileSync(p("public/icons/apple-touch-icon.png"), apple);
writeFileSync(p("app/apple-icon.png"), apple);

const ico = await pngToIco([
  await png(markSvg, 16),
  await png(markSvg, 32),
  await png(markSvg, 48),
]);
writeFileSync(p("app/favicon.ico"), ico);

console.log("brand + icons generated:");
console.log("  public/brand/rad-wordmark.{svg,png}");
console.log("  public/brand/rad-mark.svg");
console.log(`  public/icons/icon-{${SIZES.join(",")}}.png + apple-touch-icon.png`);
console.log("  app/icon.svg, app/apple-icon.png, app/favicon.ico");
