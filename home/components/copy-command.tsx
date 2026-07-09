"use client";

import { useState } from "react";

// The install-command bar: a prompt, the command, and a copy affordance.
// Cloudflare-Sandbox's `npm i …` bar, translated to a phosphor terminal.
export function CopyCommand({
  command,
  prompt = "$",
}: {
  command: string;
  prompt?: string;
}) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
      setTimeout(() => setCopied(false), 1600);
    } catch {
      /* clipboard unavailable — the command is selectable as plain text */
    }
  }

  return (
    <div className="cmd">
      <div className="cmd__body">
        <span className="cmd__prompt" aria-hidden="true">
          {prompt}
        </span>
        <code className="cmd__text">{command}</code>
      </div>
      <button
        type="button"
        className="cmd__copy"
        data-copied={copied}
        onClick={copy}
        aria-label={copied ? "Copied to clipboard" : "Copy command"}
      >
        {copied ? "COPIED" : "COPY"}
      </button>
    </div>
  );
}
