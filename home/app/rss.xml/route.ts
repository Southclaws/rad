import { buildRss } from "@/lib/rss";

export const dynamic = "force-static";

export async function GET() {
  return new Response(await buildRss(), {
    headers: { "Content-Type": "application/xml; charset=utf-8" },
  });
}
