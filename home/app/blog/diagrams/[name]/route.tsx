import { blogDiagrams } from "@/components/blog-components";
import { renderHtml } from "@/lib/render-html";
import sharp from "sharp";

export const dynamic = "force-static";
export const dynamicParams = false;

export function generateStaticParams() {
  return Object.keys(blogDiagrams).map((name) => ({ name: `${name}.png` }));
}

// Hex colors keep the SVG independent of site CSS and librsvg's CSS support.
const colors: Record<string, string> = {
  "--ink": "#e9f1eb",
  "--muted": "#a7b4ac",
  "--faint": "#77847b",
  "--green": "#5cf29b",
  "--green-mid": "#45cf82",
  "--green-deep": "#123a26",
  "--amber-mid": "#f0b966",
  "--line": "#29332d",
  "--line-2": "#414c45",
};

export async function GET(
  _request: Request,
  { params }: { params: Promise<{ name: string }> },
) {
  const { name } = await params;
  const entry = Object.entries(blogDiagrams).find(
    ([key]) => `${key}.png` === name,
  );
  if (!entry) return new Response("Not found", { status: 404 });
  const [, Diagram] = entry;
  const html = await renderHtml(<Diagram />);
  let svg = html.match(/<svg\b[\s\S]*?<\/svg>/)?.[0];
  const viewBox = svg?.match(/viewBox="0 0 ([\d.]+) ([\d.]+)"/);
  if (!svg || !viewBox) throw new Error(`Blog diagram ${name} needs a viewBox`);
  const width = Number(viewBox[1]);
  const height = Number(viewBox[2]);
  svg = svg
    .replace(
      "<svg",
      `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" font-family="monospace"`,
    )
    .replace(/var\((--[\w-]+)\)/g, (_, token: string) => {
      if (!colors[token]) throw new Error(`Unknown diagram color ${token}`);
      return colors[token];
    });
  const png = await sharp(Buffer.from(svg), { density: 144 })
    .resize(width * 2, height * 2)
    .flatten({ background: "#0d1411" })
    .png()
    .toBuffer();
  return new Response(new Uint8Array(png), {
    headers: { "Content-Type": "image/png" },
  });
}
