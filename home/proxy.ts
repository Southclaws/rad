import { createI18nMiddleware } from "fumadocs-core/i18n/middleware";
import { NextResponse, type NextFetchEvent, type NextRequest } from "next/server";
import { i18n } from "@/lib/i18n";

const localizeDocs = createI18nMiddleware(i18n);

// The marketing site remains single-language for now. Only docs participate
// in Fumadocs' locale routing, keeping existing home and blog URLs intact.
export function proxy(request: NextRequest, event: NextFetchEvent) {
  const { pathname } = request.nextUrl;
  if (pathname === "/docs" || pathname.startsWith("/docs/") || /^\/(en|fr)\/docs(?:\/|$)/.test(pathname)) {
    return localizeDocs(request, event);
  }

  return NextResponse.next();
}

export const config = {
  matcher: ["/((?!api|_next/static|_next/image|favicon.ico).*)"],
};
