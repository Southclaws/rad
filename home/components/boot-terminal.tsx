"use client";

import { useEffect, useState } from "react";
import { WindowControls } from "@/components/icons";

// The hero terminal: `$ rad` with its command menu. It types itself in, and
// if you dare click the close button, it detonates and reassembles.
const COMMANDS: [string, string][] = [
  ["serve", "run the database"],
  ["validate", "check your schema"],
  ["migrate", "reconcile changes"],
  ["generate", "emit typed clients"],
];

// reveal steps: the prompt, then one per command row
const STEPS = 1 + COMMANDS.length;

// The cursor's day job: typing dry jabs at SQL's expense, as a # comment.
const JOKES = [
  "SQL is old enough to collect a pension.",
  "joins: tables with commitment issues.",
  "NULL is not a personality.",
  "your ORM can retire now.",
  "no more SELECT * FROM regret;",
  "born in 1974, and it shows.",
  "schema first, tears never.",
  "migrations without the support group.",
];

// A typewriter that types a joke, holds, erases, and moves to the next.
function JokeLine() {
  const [i, setI] = useState(0);
  const [text, setText] = useState("");
  const [phase, setPhase] = useState<"type" | "hold" | "erase">("type");

  useEffect(() => {
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      setText(JOKES[0]);
      return;
    }
    const full = JOKES[i % JOKES.length];
    let t: ReturnType<typeof setTimeout>;
    if (phase === "type") {
      t =
        text.length < full.length
          ? setTimeout(
              () => setText(full.slice(0, text.length + 1)),
              42 + Math.random() * 45,
            )
          : setTimeout(() => setPhase("hold"), 1800);
    } else if (phase === "hold") {
      t = setTimeout(() => setPhase("erase"), 300);
    } else if (text.length > 0) {
      t = setTimeout(() => setText(full.slice(0, text.length - 1)), 22);
    } else {
      t = setTimeout(() => {
        setI((n) => (n + 1) % JOKES.length);
        setPhase("type");
      }, 260);
    }
    return () => clearTimeout(t);
  }, [text, phase, i]);

  return (
    <span className="term__comment">
      # {text}
      <span className="term__cursor" />
    </span>
  );
}

type Spark = { tx: number; ty: number; d: number; size: number; amber: boolean };

export function BootTerminal() {
  const [shown, setShown] = useState(STEPS);
  const [status, setStatus] = useState<"idle" | "boom" | "respawn">("idle");
  const [sparks, setSparks] = useState<Spark[]>([]);
  const [cycle, setCycle] = useState(0);

  // Type-in reveal, replayed on first mount and on every respawn.
  useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (reduce) {
      setShown(STEPS);
      return;
    }
    setShown(0);
    let i = 0;
    const id = setInterval(() => {
      i += 1;
      setShown(i);
      if (i >= STEPS) clearInterval(id);
    }, 200);
    return () => clearInterval(id);
  }, [cycle]);

  function explode() {
    if (status !== "idle") return;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (reduce) {
      // no shrapnel; just a quick blink out and back
      setStatus("boom");
      setTimeout(() => {
        setStatus("respawn");
        setCycle((c) => c + 1);
      }, 700);
      setTimeout(() => setStatus("idle"), 1100);
      return;
    }
    const burst: Spark[] = Array.from({ length: 30 }, (_, i) => {
      const angle = (i / 30) * Math.PI * 2 + Math.random() * 0.4;
      const dist = 90 + Math.random() * 190;
      return {
        tx: Math.cos(angle) * dist,
        ty: Math.sin(angle) * dist,
        d: Math.random() * 90,
        size: 6 + Math.random() * 9,
        amber: i % 5 === 0,
      };
    });
    setSparks(burst);
    setStatus("boom");
    setTimeout(() => {
      setStatus("respawn");
      setSparks([]);
      setCycle((c) => c + 1);
    }, 2000);
    setTimeout(() => setStatus("idle"), 2520);
  }

  const fade = (visible: boolean) => ({
    opacity: visible ? 1 : 0,
    transition: "opacity 0.15s ease",
  });

  return (
    <div className="term-wrap">
      <div
        className="term crop"
        data-status={status === "idle" ? undefined : status}
        role="img"
        aria-label="A terminal running rad, listing its commands: serve runs the database, validate checks your schema, migrate reconciles changes, generate emits typed clients."
      >
        <div className="term__bar">
          <span className="term__title">rad</span>
          <WindowControls onClose={explode} tip="nooo don't close me!" />
        </div>
        <div className="term__body" aria-hidden="true">
          <div className="term__line term__cmd" style={fade(shown >= 1)}>
            rad <span className="term__comment"># radically simple!</span>
          </div>
          <div className="term__gap" />
          {COMMANDS.map(([name, desc], idx) => (
            <div key={name} className="term__row" style={fade(shown >= idx + 2)}>
              <span className="term__cmdname">{name}</span>
              <span className="term__cmddesc">{desc}</span>
            </div>
          ))}
          <div style={fade(shown >= STEPS)}>
            <div className="term__gap" />
            <div className="term__line term__joke">
              {shown >= STEPS && <JokeLine key={cycle} />}
            </div>
          </div>
        </div>
      </div>

      {status === "boom" && (
        <div className="sparks" aria-hidden="true">
          {sparks.map((s, i) => (
            <i
              key={i}
              className="spark"
              style={
                {
                  "--tx": `${s.tx}px`,
                  "--ty": `${s.ty}px`,
                  "--d": `${s.d}ms`,
                  width: `${s.size}px`,
                  height: `${s.size}px`,
                  background: s.amber ? "var(--amber-mid)" : "var(--green)",
                } as React.CSSProperties
              }
            />
          ))}
        </div>
      )}
    </div>
  );
}
