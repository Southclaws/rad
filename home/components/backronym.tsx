"use client";

import { useEffect, useRef, useState } from "react";

// RAD is an incomplete acronym: R is Relational, D is Database, and the A is
// forever undecided. And yes, Database is ONE WORD. We ain't no "DB" shop here.
const WORDS = [
  "AAAAAAAAA",
  "AARDVARK",
  "ABI-COMPATIBLE",
  "ABSOLUTE",
  "ABSOLUTE LEGEND",
  "ABSOLUTELY LEGENDARY",
  "ABSOLUTELY NOT SQL",
  "ABSURD",
  "ACCEPTABLE",
  "ACCIDENT-PRONE",
  "ACCIDENTAL",
  "ACTUALLY NOT SQL",
  "ADEQUATE",
  "ADHD",
  "ADVERSARIAL",
  "AERODYNAMIC",
  "AESTHETIC",
  "AFFORDABLE",
  "AGGRESSIVE",
  "AI-GENERATED",
  "AI-SLOP",
  "ALIGNED",
  "ALLEGED",
  "ALLOCATED",
  "ALMOST",
  "ALPACA",
  "ALPHA-QUALITY",
  "ALPHA",
  "ALRIGHT-ISH",
  "ALRIGHT",
  "ALTERNATIVE",
  "AMBIGUOUS",
  "AMORTIZED",
  "ANARCHIC",
  "ANCHOVY",
  "ANGRY",
  "ANNOYING",
  "ANOTHER ORM",
  "ANOTHER",
  "ANTELOPE",
  "ANTI-ORM",
  "ANTI-SOCIAL",
  "ANTI-SQL",
  "ANXIOUS",
  "AOT",
  "APOCALYPTIC",
  "APPARENT",
  "APPROXIMATE",
  "ARBITRARY",
  "ARCANE",
  "ARCHAIC",
  "ARROGANT",
  "ARTISANAL",
  "ASCII",
  "ASK CHATGPT",
  "ASPIRATIONAL",
  "ASSOCIATIVE",
  "ASTONISHING",
  "ASTRAL",
  "ASTRONOMICAL",
  "ASYMPTOTIC",
  "ASYNCHRONOUS",
  "ATOMIC",
  "AUSPICIOUS",
  "AUTHENTIC",
  "AUTISTIC",
  "AUTOCOMPLETED",
  "AUTOCORRECTED",
  "AVERAGE",
  "AVOCADO",
  "AWKWARD",
  "AWW",
  "ÆSTHETIC",
  "ÆTHER",
  "ℵ",
];
const MAX = Math.max(...WORDS.map((w) => w.length));
const CHARSET = " ABCDEFGHIJKLMNOPQRSTUVWXYZ";

// One flap cell: steps forward through the charset until it lands on target,
// replaying a flap-down animation each step (real split-flaps only turn one
// way, and pass a blank between Z and A).
function Flap({ target, reduce }: { target: string; reduce: boolean }) {
  const [ch, setCh] = useState(target);
  const [step, setStep] = useState(0);
  const chRef = useRef(ch);
  chRef.current = ch;

  useEffect(() => {
    if (reduce) {
      setCh(target);
      return;
    }
    if (chRef.current === target) return;
    const id = setInterval(() => {
      const ci = Math.max(0, CHARSET.indexOf(chRef.current));
      const next = CHARSET[(ci + 1) % CHARSET.length];
      setCh(next);
      setStep((s) => s + 1);
      if (next === target) clearInterval(id);
    }, 45);
    return () => clearInterval(id);
  }, [target, reduce]);

  return (
    <span className="flap" data-blank={ch === " "} aria-hidden="true">
      <span className="flap__ch" key={step}>
        {ch === " " ? " " : ch}
      </span>
    </span>
  );
}

export function Backronym() {
  const [i, setI] = useState(0);
  const [reduce, setReduce] = useState(true);

  useEffect(() => {
    const m = window.matchMedia("(prefers-reduced-motion: reduce)");
    setReduce(m.matches);
    if (m.matches) return; // no auto-cycling when reduced motion is requested
    const id = setInterval(() => {
      setI((v) => {
        let n = v;
        while (n === v) n = Math.floor(Math.random() * WORDS.length);
        return n;
      });
    }, 3800);
    return () => clearInterval(id);
  }, []);

  const word = WORDS[i].padEnd(MAX, " ");
  const next = () => setI((v) => (v + 1) % WORDS.length);

  return (
    <div className="acro">
      <p className="acro__line">
        <span className="acro__side">
          <b>R</b>elational
        </span>
        <button
          type="button"
          className="flaps"
          onClick={next}
          aria-label={`The A in RAD currently stands for ${WORDS[i]}. For now...`}
        >
          {word.split("").map((c, idx) => (
            <Flap key={idx} target={c} reduce={reduce} />
          ))}
        </button>
        <span className="acro__side">
          <b>D</b>atabase
        </span>
      </p>
      <p className="acro__cap">(the A is still up for debate)</p>
    </div>
  );
}
