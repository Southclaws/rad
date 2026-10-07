import type { ReactNode } from "react";
import { prerender } from "react-dom/static.edge";

export async function renderHtml(node: ReactNode): Promise<string> {
  const { prelude } = await prerender(node);
  return new Response(prelude).text();
}
