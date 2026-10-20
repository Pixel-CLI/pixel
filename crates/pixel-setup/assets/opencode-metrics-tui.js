// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// Pixel's metrics display for OpenCode (V2 TUI plugin API). Shows the user,
// as a toast the model never sees, (1) the brief's own `🟩 pixel brief` block,
// which `pixel-brief.js` leaves in a per-session file, and (2) the `🟩 pixel
// <command>` block a `pixel` call printed at the end of its own output, the
// line Codex gets from its PostToolUse hook. Nothing is run or recomputed here:
// both blocks are text Pixel already printed for exactly that invocation.
// __MANAGED_BEGIN__
// __MANAGED_END__

import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// Shared with `pixel-brief.js`; keep the two spellings equal.
const BRIEF_DIR = path.join(os.tmpdir(), "pixel-opencode-brief-metrics");
const TOAST_MS = 8_000;
// The footer's tree: the identity header, then savings rows, at the very end.
const FOOTER = /(🟩 pixel [^\n]*#[0-9a-f]{6}[^\n]*\n(?:[ \t]*[│├└][^\n]*\n?)*)\s*$/u;
const FRAME_ONLY = /^\s*[│└][─\s]*$/u;
// `pixel`, `pixel-dev` or a path to either, as a command word of the call.
const PIXEL_COMMAND = /(^|[\s;&|(])(\S*\/)?pixel(-dev)?(\s|$)/u;

const tidy = (block) =>
  block.split("\n").filter((row) => row.trim() && !FRAME_ONLY.test(row)).join("\n");
// The footer block ending a tool output, frame-only rows dropped, or undefined.
const footerOf = (text) => {
  const block = FOOTER.exec(text)?.[1];
  return block && tidy(block);
};

export default {
  id: "pixel.metrics",
  setup(api) {
    if (process.argv.includes("--server")) return;
    const show = (message, sessionID) => {
      try {
        api.ui.toast.show({ message, duration: TOAST_MS, sessionID });
      } catch {
        // No toast surface: the model-visible footer is still in the tool output.
      }
    };
    // Commands of calls in flight, by call id: only a `pixel` call's own
    // footer is relayed, never a footer quoted by some other command's output.
    const commands = new Map();
    return api.data.listen((event) => {
      const { type, data } = event?.details ?? event ?? {};
      if (!data) return;
      if (type === "session.tool.called" && typeof data.input?.command === "string") {
        commands.set(data.id, data.input.command);
      } else if (type === "session.step.started") {
        const file = path.join(BRIEF_DIR, String(data.sessionID).replace(/[^\w-]/gu, "_"));
        try {
          const text = fs.readFileSync(file, "utf8");
          fs.unlinkSync(file);
          if (text.trim()) show(tidy(text), data.sessionID);
        } catch {
          // No brief block for this step.
        }
      } else if (type === "session.tool.success") {
        const command = commands.get(data.id);
        commands.delete(data.id);
        if (!command || !PIXEL_COMMAND.test(command)) return;
        const text = (data.content ?? []).map((part) => part?.text ?? "").join("\n");
        const block = footerOf(`${text}\n`);
        if (block) show(block, data.sessionID);
      }
    });
  },
};
