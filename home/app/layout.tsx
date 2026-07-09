import type { Metadata, Viewport } from "next";
import { Spline_Sans_Mono, Hanken_Grotesk } from "next/font/google";
import "./globals.css";

// Mono leads — this is a literal CLI + database, so it's earned, not costume.
const mono = Spline_Sans_Mono({
  variable: "--font-mono",
  subsets: ["latin"],
  weight: ["400", "500", "600", "700"],
  display: "swap",
});

// Humanist sans carries longer prose where continuous mono would tire.
const sans = Hanken_Grotesk({
  variable: "--font-sans",
  subsets: ["latin"],
  weight: ["400", "500", "600"],
  display: "swap",
});

const title = "Rad - rethink your relationship with rows";
const description =
  "The relational database, redesigned. With one cohesive toolchain. From schema to application.";

export const metadata: Metadata = {
  title,
  description,
  metadataBase: new URL("https://www.radengine.dev"),
  openGraph: { title, description, type: "website" },
  twitter: { card: "summary_large_image", title, description },
};

export const viewport: Viewport = {
  themeColor: "#0d1411",
  colorScheme: "dark",
};

export default function RootLayout({
  children,
}: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en" className={`${mono.variable} ${sans.variable}`}>
      <body>{children}</body>
    </html>
  );
}
