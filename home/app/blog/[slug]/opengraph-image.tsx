import { blogSource } from "@/lib/source";
import { ImageResponse } from "next/og";

export const alt = "A blog post on rad";
export const size = { width: 1200, height: 630 };
export const contentType = "image/png";

export function generateStaticParams() {
  return blogSource.generateParams().map((p) => ({ slug: (p.slug ?? []).join("/") }));
}

const BG = "#0d1411";
const INK = "#e9f1eb";
const MUTED = "#a7b4ac";
const GREEN = "#5cf29b";
const GREEN_MID = "#45cf82";
const LINE = "#333e39";

// A glyph superset so arbitrary post titles render; the css2 endpoint serves
// truetype (Satori can't use woff2) when a plain fetch subsets by `text`.
const GLYPHS = `ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 .,:;!?()[]{}#$%&*+-/=@—·…'"`;

async function loadFont(weight: number, text: string) {
  const url = `https://fonts.googleapis.com/css2?family=Spline+Sans+Mono:wght@${weight}&text=${encodeURIComponent(
    text,
  )}`;
  const css = await (await fetch(url)).text();
  const src = css.match(/src: url\((.+?)\) format\('(?:opentype|truetype)'\)/);
  if (!src) throw new Error("failed to resolve Spline Sans Mono font URL");
  return fetch(src[1]).then((r) => r.arrayBuffer());
}

export default async function Image({
  params,
}: {
  params: Promise<{ slug: string }>;
}) {
  const { slug } = await params;
  const page = blogSource.getPage([slug]);
  const title = page?.data.title ?? "rad blog";
  const date = page?.data.date
    ? new Date(page.data.date).toLocaleDateString("en-US", {
        year: "numeric",
        month: "long",
        day: "numeric",
      })
    : "";
  const meta = [date, page?.data.author].filter(Boolean).join(" · ");

  const text = GLYPHS + title + meta;
  const [regular, bold] = await Promise.all([loadFont(400, text), loadFont(700, text)]);

  return new ImageResponse(
    (
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

        <div style={{ display: "flex", alignItems: "center", fontSize: 30, fontWeight: 700 }}>
          <span style={{ color: GREEN }}>rad</span>
          <span style={{ color: MUTED, marginLeft: 14 }}>/ blog</span>
        </div>

        <div
          style={{
            display: "flex",
            fontSize: 58,
            fontWeight: 700,
            letterSpacing: -1.5,
            lineHeight: 1.1,
            color: INK,
            maxWidth: 990,
          }}
        >
          {title}
        </div>

        <div style={{ display: "flex", flexDirection: "column" }}>
          <div style={{ height: 1, background: LINE, marginBottom: 22 }} />
          <div
            style={{
              display: "flex",
              justifyContent: "space-between",
              alignItems: "center",
              fontSize: 26,
              fontWeight: 500,
            }}
          >
            <span style={{ color: MUTED }}>{meta}</span>
            <span style={{ color: GREEN_MID }}>radstack.sh</span>
          </div>
        </div>
      </div>
    ),
    {
      ...size,
      fonts: [
        { name: "Spline Sans Mono", data: regular, weight: 400, style: "normal" },
        { name: "Spline Sans Mono", data: bold, weight: 700, style: "normal" },
      ],
    },
  );
}
