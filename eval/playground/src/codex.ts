// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Codex runs for a test card: one `codex exec --json` per arm, each in its own
// CODEX_HOME so that the only difference between the arms is the brief.
//   arm "pixel": a UserPromptSubmit hook that prints the card's brief as
//                `additionalContext` (or the brief as a prompt prefix when
//                `inject` is "prefix").
//   arm "none":  no hooks, no MCP servers, nothing from pixel.
// The login is copied into each home and never read, printed or logged.
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync, copyFileSync } from "node:fs";
import { join } from "node:path";

export type Arm = "pixel" | "none";
export type RunConfig = {
  codexBin: string;
  /** Where the per-arm homes and per-run envelopes live. */
  root: string;
  /** The user's Codex home: auth.json and the model settings are copied from it. */
  userCodexHome: string;
  /** Indexed checkout Codex works in. */
  repo: string;
  /** "hook" (verified to fire under `codex exec`) or "prefix" (fallback). */
  inject: "hook" | "prefix";
  /** Hard stop for one run. */
  runTimeoutMs: number;
};

// Top-level scalars of the user's config.toml that the model behaviour depends
// on. Tables, hooks, notify, MCP servers, plugins and marketplaces are left out.
const MIRRORED = [
  "approval_policy", "model", "model_reasoning_effort", "personality",
  "service_tier", "project_doc_max_bytes", "suppress_unstable_features_warning",
];

export const mirroredSettings = (userConfig: string): string[] => {
  const out: string[] = [];
  for (const line of userConfig.split("\n")) {
    if (line.startsWith("[")) break;
    const key = /^([A-Za-z0-9_]+)\s*=/.exec(line)?.[1];
    if (key && MIRRORED.includes(key)) out.push(line.trimEnd());
  }
  return out;
};

export const homeFor = (cfg: RunConfig, arm: Arm) => join(cfg.root, `codex-home-${arm}`);

export const prepareHome = (cfg: RunConfig, arm: Arm): string => {
  const home = homeFor(cfg, arm);
  mkdirSync(home, { recursive: true, mode: 0o700 });
  chmodSync(home, 0o700);
  const auth = join(cfg.userCodexHome, "auth.json");
  if (!existsSync(auth)) throw new Error(`no Codex login at ${auth} (run: codex login)`);
  copyFileSync(auth, join(home, "auth.json"));
  chmodSync(join(home, "auth.json"), 0o600);
  const userConfig = existsSync(join(cfg.userCodexHome, "config.toml"))
    ? readFileSync(join(cfg.userCodexHome, "config.toml"), "utf8")
    : "";
  writeFileSync(
    join(home, "config.toml"),
    [
      ...mirroredSettings(userConfig),
      "check_for_update_on_startup = false",
      "",
      "[features]",
      "hooks = true",
      "plugin_hooks = false",
      "",
      `[projects."${cfg.repo}"]`,
      'trust_level = "trusted"',
      "",
    ].join("\n"),
  );
  const hooks = join(home, "hooks.json");
  if (arm === "pixel" && cfg.inject === "hook") {
    // The command reads the envelope the server wrote for this run; the path
    // reaches the hook through the environment, so the file itself is static.
    writeFileSync(
      hooks,
      JSON.stringify({ hooks: { UserPromptSubmit: [{ hooks: [{ type: "command", command: 'cat "$PLAYGROUND_BRIEF_ENVELOPE"', timeout: 10 }] }] } }),
    );
  } else writeFileSync(hooks, JSON.stringify({ hooks: {} }));
  return home;
};

export const envelope = (briefText: string) =>
  JSON.stringify({ hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: briefText } });

export const prefixed = (briefText: string, prompt: string) =>
  `Context from Pixel:\n${briefText}\n\nTask:\n${prompt}`;

export const injectionLabel = (cfg: RunConfig, arm: Arm) =>
  arm === "none"
    ? "initial prompt only"
    : cfg.inject === "hook"
      ? "brief injected by a UserPromptSubmit hook"
      : "brief injected as prompt prefix (exec has no hook)";

type Run = { proc: ReturnType<typeof Bun.spawn>; arm: Arm; aborted: boolean };
const runs = new Map<string, Set<Run>>();

const killGroup = (run: Run) => {
  const pid = run.proc.pid;
  try {
    process.kill(-pid, "SIGTERM");
  } catch {
    /* already gone */
  }
  setTimeout(() => {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      /* gone */
    }
  }, 1500).unref();
};

/** Kill the process group of every run of a card. Returns how many were live. */
export const abortCard = (cardId: string): number => {
  const set = runs.get(cardId);
  if (!set) return 0;
  for (const r of set) {
    r.aborted = true;
    killGroup(r);
  }
  return set.size;
};

const sse = (event: string, data: unknown) =>
  `event: ${event}\ndata: ${typeof data === "string" ? data : JSON.stringify(data)}\n\n`;

export const startRun = (
  cfg: RunConfig,
  req: { cardId: string; prompt: string; arm: Arm; briefText: string },
): Response => {
  const { cardId, prompt, arm, briefText } = req;
  let home: string;
  try {
    home = prepareHome(cfg, arm);
  } catch (e) {
    return Response.json({ ok: false, error: String(e instanceof Error ? e.message : e) }, { status: 500 });
  }
  // One run per card and arm: a second Send replaces the first.
  for (const r of runs.get(cardId) ?? []) if (r.arm === arm) killGroup(r);

  const args = ["exec", "--json", "-s", "read-only", "--skip-git-repo-check", "-C", cfg.repo];
  const env: Record<string, string> = { ...(process.env as Record<string, string>), CODEX_HOME: home, PIXEL_BRIEF: "0" };
  delete env.ANTHROPIC_API_KEY;
  let text = prompt;
  if (arm === "pixel") {
    if (cfg.inject === "hook") {
      const dir = join(cfg.root, "runs");
      mkdirSync(dir, { recursive: true });
      const file = join(dir, `${cardId.replace(/[^\w-]/g, "_")}-${Date.now()}.json`);
      writeFileSync(file, envelope(briefText));
      env.PLAYGROUND_BRIEF_ENVELOPE = file;
      args.push("--dangerously-bypass-hook-trust");
    } else text = prefixed(briefText, prompt);
  }
  args.push(text);

  const t0 = performance.now();
  const proc = Bun.spawn([cfg.codexBin, ...args], {
    cwd: cfg.repo, env, stdin: "ignore", stdout: "pipe", stderr: "pipe", detached: true,
  });
  const run: Run = { proc, arm, aborted: false };
  if (!runs.has(cardId)) runs.set(cardId, new Set());
  runs.get(cardId)!.add(run);
  const timer = setTimeout(() => killGroup(run), cfg.runTimeoutMs);

  const enc = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    async start(ctl) {
      const send = (event: string, data: unknown) => {
        try {
          ctl.enqueue(enc.encode(sse(event, data)));
        } catch {
          /* client went away */
        }
      };
      send("meta", { arm, mode: arm === "none" ? "none" : cfg.inject, label: injectionLabel(cfg, arm) });
      const pump = async (stream: ReadableStream<Uint8Array>, onLine: (l: string) => void) => {
        const dec = new TextDecoder();
        let buf = "";
        for await (const chunk of stream) {
          buf += dec.decode(chunk, { stream: true });
          let i: number;
          while ((i = buf.indexOf("\n")) >= 0) {
            const line = buf.slice(0, i).trim();
            buf = buf.slice(i + 1);
            if (line) onLine(line);
          }
        }
        if (buf.trim()) onLine(buf.trim());
      };
      const errTail: string[] = [];
      await Promise.all([
        pump(proc.stdout as ReadableStream<Uint8Array>, (l) => send("codex", l)),
        pump(proc.stderr as ReadableStream<Uint8Array>, (l) => {
          errTail.push(l);
          if (errTail.length > 20) errTail.shift();
        }),
      ]);
      const code = await proc.exited;
      clearTimeout(timer);
      runs.get(cardId)?.delete(run);
      if (runs.get(cardId)?.size === 0) runs.delete(cardId);
      send("end", {
        code, aborted: run.aborted, ms: Math.round(performance.now() - t0),
        stderr: code === 0 ? "" : errTail.join("\n").slice(-600),
      });
      try {
        ctl.close();
      } catch {
        /* closed */
      }
    },
    cancel() {
      run.aborted = true;
      killGroup(run);
    },
  });
  return new Response(body, {
    headers: { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" },
  });
};
