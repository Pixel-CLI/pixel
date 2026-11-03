// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Self-test of the Codex run endpoint against a fake `codex` that prints canned
// JSONL events, records how it was called, and can hang to test the abort.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { handlers, type Config } from "./api";
import { abortCard, mirroredSettings, prepareHome } from "./codex";
import { applyEvent, classify, emptyPane, paneStats, unwrapShell } from "./events";

const EVENTS = [
  { type: "thread.started", thread_id: "t" },
  { type: "item.completed", item: { id: "i0", type: "error", message: "Codex is ignoring 1 unrecognized configuration setting." } },
  { type: "turn.started" },
  { type: "item.started", item: { id: "i1", type: "command_execution", command: "/bin/zsh -lc \"rg -n timeout crates\"", status: "in_progress" } },
  { type: "item.completed", item: { id: "i1", type: "command_execution", command: "/bin/zsh -lc \"rg -n timeout crates\"", status: "completed", exit_code: 0 } },
  { type: "item.completed", item: { id: "i2", type: "command_execution", command: "/bin/zsh -lc \"sed -n '1,40p' a.rs\"", status: "failed", exit_code: 1 } },
  { type: "item.completed", item: { id: "i3", type: "agent_message", text: "the answer" } },
  { type: "turn.completed", usage: { input_tokens: 100, cached_input_tokens: 40, output_tokens: 7, reasoning_output_tokens: 3 } },
];

let dir: string;
let cfg: Config;
let api: ReturnType<typeof handlers>;
let calls: string;

const post = (path: string, body: unknown) => new Request(`http://x${path}`, { method: "POST", body: JSON.stringify(body) });

const readSse = async (res: Response) => {
  const text = await res.text();
  return text.split("\n\n").filter(Boolean).map((f) => ({
    event: /^event: (.*)$/m.exec(f)![1]!,
    data: f.split("\n").filter((l) => l.startsWith("data: ")).map((l) => l.slice(6)).join("\n"),
  }));
};
const lastCall = () => JSON.parse(readFileSync(calls, "utf8").trim().split("\n").at(-1)!) as {
  args: string[]; home: string; envelope: string | null; hasEnvVar: boolean; pixelBrief: string;
};
const run = (arm: string, extra: Record<string, unknown> = {}) =>
  api.run(post("/api/run", { cardId: "c1", prompt: "which setting?", arm, briefText: "[PIXEL:BRIEF]\nkind: lookup", ...extra }));

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "playground-codex-"));
  calls = join(dir, "calls.jsonl");
  process.env.FAKE_CALLS = calls;
  process.env.FAKE_EVENTS = join(dir, "events.jsonl");
  writeFileSync(process.env.FAKE_EVENTS, EVENTS.map((e) => JSON.stringify(e)).join("\n"));
  mkdirSync(join(dir, "user"));
  writeFileSync(join(dir, "user/auth.json"), '{"secret":"do-not-print"}');
  writeFileSync(join(dir, "user/config.toml"), [
    'approval_policy = "never"', 'model = "gpt-test"', 'model_reasoning_effort = "high"',
    'notify = ["/bin/notify"]', "", "[features]", "hooks = true", "", "[mcp_servers.x]", 'url = "http://x"', "",
    "[hooks.pixel]", 'command = "pixel"',
  ].join("\n"));
  mkdirSync(join(dir, "repo"));
  const bin = join(dir, "fake-codex");
  writeFileSync(bin, `#!/bin/sh
prompt=""; for a in "$@"; do prompt="$a"; done
env_file="$PLAYGROUND_BRIEF_ENVELOPE"
body=""; [ -n "$env_file" ] && body=$(cat "$env_file")
printf '{"args":%s,"home":"%s","envelope":%s,"hasEnvVar":%s,"pixelBrief":"%s"}\\n' \\
  "$(printf '%s\\n' "$@" | python3 -c 'import sys,json;print(json.dumps(sys.stdin.read().rstrip("\\n").split("\\n")))')" \\
  "$CODEX_HOME" "$(printf '%s' "$body" | python3 -c 'import sys,json;d=sys.stdin.read();print(json.dumps(d) if d else "null")')" \\
  "$([ -n "$env_file" ] && echo true || echo false)" "$PIXEL_BRIEF" >> "$FAKE_CALLS"
case "$prompt" in
  *HANG*) sleep 30 & echo $! > "$FAKE_CHILD"; wait ;;
  *) cat "$FAKE_EVENTS"; echo ;;
esac
`);
  chmodSync(bin, 0o755);
  cfg = {
    repo: join(dir, "repo"), promptsFile: "", localPromptsFile: "", binsFile: "", defaultBins: [], timeoutMs: 1000,
    codex: { codexBin: bin, root: join(dir, "play"), userCodexHome: join(dir, "user"), repo: join(dir, "repo"), inject: "hook", runTimeoutMs: 20_000 },
  };
  api = handlers(cfg);
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("codex home isolation", () => {
  test("mirrors the model settings only, never hooks, MCP, notify or tables", () => {
    expect(mirroredSettings(readFileSync(join(dir, "user/config.toml"), "utf8"))).toEqual([
      'approval_policy = "never"', 'model = "gpt-test"', 'model_reasoning_effort = "high"',
    ]);
    const home = prepareHome(cfg.codex, "none");
    const toml = readFileSync(join(home, "config.toml"), "utf8");
    expect(toml).toContain('model = "gpt-test"');
    expect(toml).toContain(`[projects."${cfg.codex.repo}"]`);
    for (const banned of ["notify", "mcp_servers", "pixel", "[hooks"]) expect(toml).not.toContain(banned);
    expect(readFileSync(join(home, "auth.json"), "utf8")).toContain("do-not-print");
    expect(statSync(join(home, "auth.json")).mode & 0o777).toBe(0o600);
  });
  test("only the pixel arm has a hook, and only UserPromptSubmit", () => {
    const none = JSON.parse(readFileSync(join(prepareHome(cfg.codex, "none"), "hooks.json"), "utf8"));
    expect(none).toEqual({ hooks: {} });
    const pixel = JSON.parse(readFileSync(join(prepareHome(cfg.codex, "pixel"), "hooks.json"), "utf8"));
    expect(Object.keys(pixel.hooks)).toEqual(["UserPromptSubmit"]);
  });
  test("a missing login is a clear error", async () => {
    const res = await handlers({ ...cfg, codex: { ...cfg.codex, userCodexHome: join(dir, "nowhere") } }).run(
      post("/api/run", { cardId: "x", prompt: "p", arm: "none" }),
    );
    expect(res.status).toBe(500);
    expect((await res.json()).error).toContain("codex login");
  });
});

describe("POST /api/run", () => {
  test("validates the request", async () => {
    expect((await run("pixel", { briefText: "  " })).status).toBe(400);
    expect((await run("both")).status).toBe(400);
    expect((await run("none", { prompt: "" })).status).toBe(400);
  });
  test("pixel arm streams the events and hands the brief to the hook envelope", async () => {
    const frames = await readSse(await run("pixel"));
    expect(frames[0]).toMatchObject({ event: "meta" });
    expect(JSON.parse(frames[0]!.data)).toMatchObject({ arm: "pixel", mode: "hook" });
    expect(frames.filter((f) => f.event === "codex")).toHaveLength(EVENTS.length);
    expect(JSON.parse(frames.at(-1)!.data)).toMatchObject({ code: 0, aborted: false });
    const c = lastCall();
    expect(c.args).toEqual(expect.arrayContaining(["exec", "--json", "-s", "read-only", "--dangerously-bypass-hook-trust", "-C", cfg.codex.repo]));
    expect(c.args.at(-1)).toBe("which setting?");
    expect(c.home).toBe(join(dir, "play/codex-home-pixel"));
    expect(JSON.parse(c.envelope!)).toEqual({ hookSpecificOutput: { hookEventName: "UserPromptSubmit", additionalContext: "[PIXEL:BRIEF]\nkind: lookup" } });
    expect(c.pixelBrief).toBe("0");
  });
  test("none arm gets the prompt only: no envelope, no hook-trust bypass", async () => {
    await readSse(await run("none"));
    const c = lastCall();
    expect(c.hasEnvVar).toBe(false);
    expect(c.args).not.toContain("--dangerously-bypass-hook-trust");
    expect(c.args.at(-1)).toBe("which setting?");
    expect(c.home).toBe(join(dir, "play/codex-home-none"));
  });
  test("prefix mode prepends the brief and labels the pane", async () => {
    const prefix = handlers({ ...cfg, codex: { ...cfg.codex, inject: "prefix" } });
    const frames = await readSse(await prefix.run(post("/api/run", { cardId: "c2", prompt: "which setting?", arm: "pixel", briefText: "BRIEF" })));
    expect(JSON.parse(frames[0]!.data).label).toBe("brief injected as prompt prefix (exec has no hook)");
    const c = lastCall();
    // The fake splits its argv on newlines, so the multi-line prompt is the tail.
    expect(c.args.join("\n").endsWith("Context from Pixel:\nBRIEF\n\nTask:\nwhich setting?")).toBe(true);
    expect(c.hasEnvVar).toBe(false);
  });
});

describe("POST /api/abort", () => {
  test("kills the whole process group, children included", async () => {
    process.env.FAKE_CHILD = join(dir, "child.pid");
    const res = await run("none", { cardId: "hang", prompt: "HANG" });
    const done = readSse(res);
    for (let i = 0; i < 50 && !existsSync(process.env.FAKE_CHILD); i++) await Bun.sleep(100);
    const pid = Number(readFileSync(process.env.FAKE_CHILD, "utf8"));
    expect(() => process.kill(pid, 0)).not.toThrow();
    const out = await (await api.abort(post("/api/abort", { cardId: "hang" }))).json();
    expect(out).toEqual({ ok: true, killed: 1 });
    const frames = await done;
    expect(JSON.parse(frames.at(-1)!.data)).toMatchObject({ aborted: true });
    await Bun.sleep(200);
    expect(() => process.kill(pid, 0)).toThrow();
    expect(abortCard("hang")).toBe(0);
  });
});

describe("event folding", () => {
  test("tools, answer, usage and the search/read split", () => {
    const s = emptyPane();
    for (const e of EVENTS) applyEvent(s, e as Record<string, unknown>);
    expect(s.tools.map((t) => [t.label, t.kind, t.status])).toEqual([
      ["rg -n timeout crates", "search", "done"],
      ["sed -n '1,40p' a.rs", "read", "failed"],
    ]);
    expect(s.answer).toBe("the answer");
    expect(s.error).toBeNull();
    expect(s.usage).toEqual({ input: 100, cached: 40, output: 7, reasoning: 3 });
    expect(paneStats(s)).toEqual({ calls: 2, searches: 1, reads: 1, tokens: 107 });
  });
  test("shell unwrapping and classification", () => {
    expect(unwrapShell(`/bin/zsh -lc 'cat a | head'`)).toBe("cat a | head");
    expect(classify("cat a.rs | head -5")).toBe("read");
    expect(classify("cd x && rg foo")).toBe("search");
    expect(classify("ls -la")).toBe("other");
    expect(classify("git status")).toBe("other");
  });
  test("a failed turn surfaces its error", () => {
    const s = applyEvent(emptyPane(), { type: "turn.failed", error: { message: "boom" } });
    expect(s.error).toBe("boom");
  });
});
