import { ImageResponse } from "next/og";

// Social share card, rendered by next/og (Satori). The schematic-terminal
// system, translated to a 1200×630 still: phosphor green on ink, the extruded
// wordmark, the hero line, and a prompt/URL footer under a hairline.
export const alt =
  "rad. Rethink your relationship with rows. The relational database, redesigned. With one cohesive toolchain. From schema to application.";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

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
async function loadFont(weight: number, text: string) {
  const url = `https://fonts.googleapis.com/css2?family=Spline+Sans+Mono:wght@${weight}&text=${encodeURIComponent(
    text,
  )}`;
  const css = await (await fetch(url)).text();
  const src = css.match(/src: url\((.+?)\) format\('(?:opentype|truetype)'\)/);
  if (!src) throw new Error("failed to resolve Spline Sans Mono font URL");
  return fetch(src[1]).then((r) => r.arrayBuffer());
}

export default async function OpengraphImage() {
  const text = WORDMARK + HEADLINE + SUB + PROMPT + URLTAG + "$v0";
  const [regular, bold] = await Promise.all([
    loadFont(400, text),
    loadFont(700, text),
  ]);

  return new ImageResponse(
    <div
      style={{
        height: "100%",
        width: "100%",
        display: "flex",
        flexDirection: "column",
        justifyContent: "space-between",
        background: BG,
        padding: 72,
        fontFamily: "Spline Sans Mono",
        position: "relative",
      }}
    >
      {/* faint blueprint frame */}
      <div
        style={{
          position: "absolute",
          top: 36,
          left: 36,
          right: 36,
          bottom: 36,
          border: `1px solid ${LINE}`,
        }}
      />

      {/* wordmark + block cursor */}
      <div style={{ display: "flex", alignItems: "flex-end" }}>
        <div
          style={{
            fontSize: 168,
            fontWeight: 700,
            lineHeight: 1,
            letterSpacing: -8,
            color: GREEN,
            textShadow: `6px 6px 0 ${GREEN_DEEP}, 12px 12px 0 ${GREEN_DEEP}`,
          }}
        >
          {WORDMARK}
        </div>
      </div>

      {/* headline + sub */}
      <div style={{ display: "flex", flexDirection: "column" }}>
        <div
          style={{
            fontSize: 54,
            fontWeight: 700,
            letterSpacing: -1.5,
            color: INK,
          }}
        >
          {HEADLINE}
        </div>
        <div
          style={{
            marginTop: 16,
            fontSize: 30,
            fontWeight: 400,
            color: MUTED,
          }}
        >
          {SUB}
        </div>
      </div>

      {/* footer: prompt · url, under a hairline */}
      <div style={{ display: "flex", flexDirection: "column" }}>
        <div style={{ height: 1, background: LINE, marginBottom: 22 }} />
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            fontSize: 27,
            fontWeight: 500,
          }}
        >
          <div style={{ display: "flex", color: INK }}>
            <span style={{ color: GREEN_MID, marginRight: 12 }}>$</span>
            {PROMPT}
          </div>
          <div style={{ color: GREEN_MID }}>{URLTAG}</div>
        </div>
      </div>
    </div>,
    {
      ...size,
      fonts: [
        {
          name: "Spline Sans Mono",
          data: regular,
          weight: 400,
          style: "normal",
        },
        { name: "Spline Sans Mono", data: bold, weight: 700, style: "normal" },
      ],
    },
  );
}
