// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// pixel OpenCode plugin — injects the pixel retrieval protocol into the
// system prompt every turn. Reads the generated PIXEL.md shipped at the
// plugin root; no network access, no hooks that block tool calls.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
let instructions = "";
try {
  instructions = readFileSync(join(here, "..", "..", "PIXEL.md"), "utf8");
} catch {
  // PIXEL.md missing — stay silent rather than error on every turn.
}

export default async () => ({
  "experimental.chat.system.transform": async (_input, output) => {
    if (!instructions) return;
    if (output.system.length > 0) {
      output.system[output.system.length - 1] += "\n\n" + instructions;
    } else {
      output.system.push(instructions);
    }
  },
});
