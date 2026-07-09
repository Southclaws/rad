"use client";

import { useEffect, useRef } from "react";

// Reveal-on-scroll that is safe by construction: the element ships fully
// visible (SSR, no-JS, reduced-motion, headless). Only after mount does JS
// opt in to the entrance, and a fallback timer guarantees it always ends
// visible even if the observer never fires (hidden tab, headless render).
export function Reveal({
  children,
  as: Tag = "div",
  className = "",
  delay = 0,
  style,
}: {
  children: React.ReactNode;
  as?: "div" | "section";
  className?: string;
  delay?: number;
  style?: React.CSSProperties;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;

    const reduce = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    if (reduce || !("IntersectionObserver" in window)) return;

    el.setAttribute("data-anim", "");
    if (delay) el.style.transitionDelay = `${delay}ms`;

    const show = () => el.setAttribute("data-shown", "true");
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) {
            show();
            io.disconnect();
          }
        }
      },
      { rootMargin: "0px 0px -12% 0px", threshold: 0.05 },
    );
    io.observe(el);

    const fallback = setTimeout(show, 1400);
    return () => {
      io.disconnect();
      clearTimeout(fallback);
    };
  }, [delay]);

  return (
    <Tag ref={ref as never} className={`reveal ${className}`.trim()} style={style}>
      {children}
    </Tag>
  );
}
