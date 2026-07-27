"use client";

import { useEffect, useState } from "react";

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

type OS = "windows" | "mac" | "linux";

// One install command per OS; the script resolves the CPU architecture itself,
// so OS is the only axis. macOS and Linux share the POSIX installer.
const COMMANDS: Record<OS, { label: string; command: string; prompt: string }> = {
  windows: {
    label: "Windows",
    command:
      'powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://radengine.dev/install.ps1 | iex"',
    prompt: ">",
  },
  mac: {
    label: "macOS",
    command: "curl -fsSL https://radengine.dev/install.sh | sh",
    prompt: "$",
  },
  linux: {
    label: "Linux",
    command: "curl -fsSL https://radengine.dev/install.sh | sh",
    prompt: "$",
  },
};

const OS_ORDER: OS[] = ["windows", "mac", "linux"];

// detectOS reads the browser's user-agent — a rough guess, not authoritative;
// it only picks the default tab, and the installer resolves the real target.
function detectOS(): OS {
  if (typeof navigator === "undefined") return "mac";
  const ua = navigator.userAgent;
  if (/Windows|Win32|Win64/i.test(ua)) return "windows";
  if (/Mac|iPhone|iPad|iPod/i.test(ua)) return "mac";
  return "linux";
}

// InstallCommand is the copy bar with an OS switch (Windows / macOS / Linux). It
// starts on macOS so the server and first client render agree, then snaps to
// the browser's detected OS after mount; each tab swaps the shown command.
export function InstallCommand() {
  const [os, setOS] = useState<OS>("mac");
  useEffect(() => setOS(detectOS()), []);
  const active = COMMANDS[os];

  return (
    <div className="install">
      <div className="install__tabs" role="group" aria-label="Operating system">
        {OS_ORDER.map((key) => (
          <button
            key={key}
            type="button"
            className="install__tab"
            data-active={key === os}
            aria-pressed={key === os}
            onClick={() => setOS(key)}
          >
            {COMMANDS[key].label}
          </button>
        ))}
      </div>
      <CopyCommand command={active.command} prompt={active.prompt} />
    </div>
  );
}
