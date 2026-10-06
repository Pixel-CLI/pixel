// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// Run with `bun scripts/test-pi-impact.mjs`.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const temp = mkdtempSync(join(tmpdir(), "pi-impact-"));
let passed = 0;
try {
  const source = readFileSync(new URL("../pi/extensions/pixel-impact.ts", import.meta.url), "utf8")
    .replace('import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";', "")
    .replace("(pi: ExtensionAPI)", "(pi)")
    .replace('const PIXEL_BIN = "pixel";', 'const PIXEL_BIN = "/fixture/pixel";');
  const extensionPath = join(temp, "pi-impact.mjs");
  writeFileSync(extensionPath, source);
  const { default: activate } = await import(pathToFileURL(extensionPath).href);
  const check = async (name, run) => {
    await run();
    passed += 1;
    console.log(`ok ${passed} - ${name}`);
  };
  const host = (result = { code: 0, killed: false, stdout: '{"callers":[]}' }) => {
    const commands = [];
    const messages = [];
    const notifications = [];
    const events = [];
    const api = {
      on: (event) => events.push(event),
      registerCommand: (name, options) => commands.push({ name, ...options }),
      exec: async (...args) => { commands.execArgs = args; return result; },
      sendMessage: (...args) => messages.push(args),
    };
    activate(api);
    return {
      commands, messages, notifications, events,
      ctx: { cwd: "/repo", signal: new AbortController().signal, ui: { notify: (...args) => notifications.push(args) } },
    };
  };

  await check("extension registers only an explicit slash command while Pi loads it", () => {
    const h = host();
    assert.deepEqual(h.commands.map(({ name }) => name), ["pixel-impact"]);
    assert.deepEqual(h.events, [], "no lifecycle handler: nothing runs until the user types the command");
  });

  await check("package includes both Claude skills and the distributed Pi command", () => {
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
    assert.ok(pkg.files.includes("claude-skills/"));
    assert.ok(pkg.files.includes("pi/"));
    assert.deepEqual(pkg.pi.extensions, ["./pi/extensions/pixel-impact.ts"]);
    assert.deepEqual(pkg.pi.skills, [], "Pi must not auto-discover root skill packages");
    const distributed = readFileSync(new URL("../pi/extensions/pixel-impact.ts", import.meta.url), "utf8");
    const installerAsset = readFileSync(new URL("../crates/pixel-install/assets/pi-impact.ts", import.meta.url), "utf8")
      .replace('const PIXEL_BIN = __PIXEL_BIN__;', 'const PIXEL_BIN = "pixel";')
      .replace("// __MANAGED_BEGIN__\n", "")
      .replace("// __MANAGED_END__\n", "");
    assert.equal(distributed, installerAsset, "installer and package extensions stay in sync");
    assert.match(distributed, /const PIXEL_BIN = "pixel";/);
    assert.doesNotMatch(distributed, /__PIXEL_BIN__|__MANAGED_(?:BEGIN|END)__/);
  });

  await check("command passes one symbol argv and uses bounded read-only existing-graph flags", async () => {
    const h = host();
    await h.commands[0].handler("transferPageToGhost", h.ctx);
    assert.equal(h.commands.execArgs[0], "/fixture/pixel");
    assert.deepEqual(h.commands.execArgs[1], ["impact", "transferPageToGhost", "--no-refresh", "--depth", "2", "--json", "--metrics", "off"]);
    assert.equal(h.commands.execArgs[2].cwd, "/repo");
    assert.equal(h.commands.execArgs[2].timeout, 1800);
    assert.match(h.messages[0][0].content, /\{"callers":\[\]\}/);
    assert.equal(h.messages[0][1].triggerTurn, false);
  });

  await check("quoted symbols stay a single argument without shell evaluation", async () => {
    const h = host();
    await h.commands[0].handler("'transferPageToGhost; touch /tmp/should-not-run'", h.ctx);
    assert.equal(h.commands.execArgs[1][1], "transferPageToGhost; touch /tmp/should-not-run");
    assert.equal(h.commands.execArgs[1].length, 8);
  });

  await check("missing, stale, unsupported, failed, or timed-out Pixel falls back natively", async () => {
    for (const result of [
      { code: 2, killed: false, stdout: "", stderr: "unknown --no-refresh" },
      { code: null, killed: true, stdout: "", stderr: "" },
      { code: 0, killed: false, stdout: "", stderr: "" },
    ]) {
      const h = host(result);
      await h.commands[0].handler("transferPageToGhost", h.ctx);
      assert.match(h.messages[0][0].content, /Continue with native search/);
      assert.match(h.messages[0][0].content, /no index refresh was attempted/);
    }
  });

  await check("empty, option-like, multiline, and oversized symbols do not run Pixel", async () => {
    for (const symbol of ["", "--help", "a\nb", "x".repeat(513)]) {
      const h = host();
      await h.commands[0].handler(symbol, h.ctx);
      assert.equal(h.commands.execArgs, undefined);
      assert.equal(h.notifications.length, 1);
    }
  });

  await check("large output is bounded before adding it to session context", async () => {
    const h = host({ code: 0, killed: false, stdout: JSON.stringify({ padding: "x".repeat(20_000) }) });
    await h.commands[0].handler("KnownSymbol", h.ctx);
    assert.match(h.messages[0][0].content, /exceeds the 12000-byte display limit/);
    assert.doesNotMatch(h.messages[0][0].content, /stale|unavailable/i);
    assert.equal(h.messages[0][1].triggerTurn, false);
  });
} finally {
  rmSync(temp, { recursive: true, force: true });
}
