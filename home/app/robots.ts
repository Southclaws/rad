import type { MetadataRoute } from "next";

const BASE = "https://radstack.sh";

export default function robots(): MetadataRoute.Robots {
  return {
    rules: { userAgent: "*", allow: "/" },
    sitemap: `${BASE}/sitemap.xml`,
    host: BASE,
  };
}
