// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Self-test of the playground API against a fake `pixel` that prints canned
// output. No indexed repository and no real binary needed.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { handlers, type Config } from "./api";
import { parseBrief, parseRange } from "./brief";

const BRIEF = [
  "[PIXEL:BRIEF]",
  "kind: lookup",
  "files: a/b.rs:12 — fn one() {; c/d.md:3 — see `x;y` here (+2 more)",
  "searched: content for foo, bar (2 terms, 10 files)",
  "confidence: low — possibly related",
  "read: a/b.rs:10-20 — one",
  "also: e/f.rs:5-6 — two",
  "coverage: 1/1 ops answered",
].join("\n");
const RECORD = {
  brief: BRIEF, bytes: 321, tier: "low", score: 0.8, gate: "open", signal: "weak", kind: "lookup",
  receipt: true, route: "daemon", elapsed_ms: 12, reason: null,
  features: { struct_ratio: 0.5, struct_per_mille: 4, best_structural_weight: 9, total_weight: 13, keywords: 6 },
};

let dir: string;
let cfg: Config;
let api: ReturnType<typeof handlers>;

const script = (name: string, body: string) => {
  const p = join(dir, name);
  writeFileSync(p, `#!/bin/sh\n${body}\n`);
  chmodSync(p, 0o755);
  return p;
};
const post = (path: string, body: unknown) =>
  new Request(`http://x${path}`, { method: "POST", body: JSON.stringify(body) });

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "playground-"));
  mkdirSync(join(dir, "repo/a"), { recursive: true });
  writeFileSync(join(dir, "repo/a/b.rs"), Array.from({ length: 30 }, (_, i) => `line ${i + 1}`).join("\n"));
  writeFileSync(
    join(dir, "prompts.jsonl"),
    [
      JSON.stringify({ id: "p-1", text: "first", lang: "en", on_topic: true, kind: "plain", expected_files: ["a/b.rs"] }),
      "not json",
      JSON.stringify({ id: "p-2", text: "second", lang: "fr", on_topic: false, category: "ops" }),
    ].join("\n"),
  );
  const fakeJson = script("fake-json", `case "$*" in *--help*) echo "  --json  print json";; *) cat <<'JSON'\n${JSON.stringify(RECORD)}\nJSON\n;; esac`);
  const fakeOld = script("fake-old", `case "$*" in *--help*) echo "no flags";; *--json*) echo "error: unexpected argument '--json' found" >&2; exit 2;; *) printf '%s\\n' '[PIXEL:BRIEF]' 'kind: bugfix';; esac`);
  const fakeSlow = script("fake-slow", `sleep 10`);
  const fakeBad = script("fake-bad", `case "$*" in *--help*) echo "--json";; *) echo boom >&2; exit 3;; esac`);
  cfg = {
    repo: join(dir, "repo"),
    promptsFile: join(dir, "prompts.jsonl"),
    localPromptsFile: join(dir, "local.json"),
    binsFile: join(dir, "bins.json"),
    defaultBins: [
      { name: "json", path: fakeJson },
      { name: "old", path: fakeOld },
      { name: "slow", path: fakeSlow },
      { name: "bad", path: fakeBad },
      { name: "gone", path: join(dir, "missing") },
    ],
    timeoutMs: 400,
    codex: { codexBin: "/bin/false", root: dir, userCodexHome: dir, repo: join(dir, "repo"), inject: "hook", runTimeoutMs: 5000 },
  };
  api = handlers(cfg);
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("brief parsing", () => {
  test("splits files entries on path-shaped boundaries only", () => {
    const p = parseBrief(BRIEF);
    expect(p.files.map((r) => `${r.path}:${r.start}`)).toEqual(["a/b.rs:12", "c/d.md:3"]);
    expect(p.files[1]!.label).toBe("see `x;y` here");
    expect(p.read).toEqual([{ path: "a/b.rs", start: 10, end: 20, label: "one" }]);
    expect(p.also[0]!.path).toBe("e/f.rs");
    expect(p.confidenceLevel).toBe("low");
    expect(p.searched).toContain("2 terms");
    expect(p.header).toBe(true);
  });
  test("a range without an end spans one line", () => {
    expect(parseRange("x.rs:7")).toEqual({ path: "x.rs", start: 7, end: 7, label: "" });
    expect(parseRange("no range")).toBeNull();
  });
});

describe("GET /api/prompts", () => {
  test("lists the corpus, skips bad lines, maps category to kind", async () => {
    const rows = await (await api.prompts()).json();
    expect(rows.map((r: { id: string }) => r.id)).toEqual(["p-1", "p-2"]);
    expect(rows[1].kind).toBe("ops");
    expect(rows[0].expected_files).toEqual(["a/b.rs"]);
  });
  test("added prompts persist and are marked local", async () => {
    const bad = await api.addPrompt(post("/api/prompts", { text: "  " }));
    expect(bad.status).toBe(400);
    const res = await (await api.addPrompt(post("/api/prompts", { text: " mine ", lang: "fr" }))).json();
    expect(res.prompt).toMatchObject({ id: "local-001", text: "mine", lang: "fr", local: true });
    const rows = await (await api.prompts()).json();
    expect(rows.at(-1)).toMatchObject({ id: "local-001", local: true });
    const again = await (await api.addPrompt(post("/api/prompts", { text: "two" }))).json();
    expect(again.prompt.id).toBe("local-002");
  });
});

describe("GET /api/bins", () => {
  test("probes --json support and existence", async () => {
    const { bins } = await (await api.bins()).json();
    const by = Object.fromEntries(bins.map((b: { name: string }) => [b.name, b]));
    expect(by.json).toMatchObject({ exists: true, json: true });
    expect(by.old).toMatchObject({ exists: true, json: false });
    expect(by.gone).toMatchObject({ exists: false, json: null });
  });
  test("bins.json overrides the defaults", async () => {
    writeFileSync(cfg.binsFile, JSON.stringify([{ name: "only", path: cfg.defaultBins[0]!.path }]));
    const { bins } = await (await api.bins()).json();
    expect(bins.map((b: { name: string }) => b.name)).toEqual(["only"]);
    rmSync(cfg.binsFile);
  });
});

describe("POST /api/brief", () => {
  const brief = async (bin: string, prompt = "hello") => {
    const res = await api.brief(post("/api/brief", { prompt, bin }));
    return { status: res.status, body: await res.json() };
  };
  test("returns the brief, parsed ranges and the decision record", async () => {
    const { status, body } = await brief(cfg.defaultBins[0]!.path);
    expect(status).toBe(200);
    expect(body).toMatchObject({ ok: true, json: true, bytes: 321, cap_bytes: 2048 });
    expect(body.parsed.read[0].path).toBe("a/b.rs");
    expect(body.decision).toMatchObject({ tier: "low", gate: "open", score: 0.8, receipt: true });
    expect(body.decision.features.struct_ratio).toBe(0.5);
    expect(body.latency_ms).toBeGreaterThanOrEqual(0);
  });
  test("a binary without --json falls back to raw text", async () => {
    const { body } = await brief(cfg.defaultBins[1]!.path);
    expect(body).toMatchObject({ ok: true, json: false, decision: null });
    expect(body.brief).toBe("[PIXEL:BRIEF]\nkind: bugfix");
    expect(body.note).toContain("no `brief --json`");
  });
  test("times out, reports exit codes, rejects unknown or missing binaries", async () => {
    expect((await brief(cfg.defaultBins[2]!.path)).status).toBe(504);
    const bad = await brief(cfg.defaultBins[3]!.path);
    expect(bad.status).toBe(502);
    expect(bad.body.error).toContain("boom");
    expect((await brief(cfg.defaultBins[4]!.path)).status).toBe(404);
    expect((await brief("/bin/echo")).status).toBe(404);
    expect((await brief(cfg.defaultBins[0]!.path, " ")).status).toBe(400);
  });
});

describe("GET /api/file", () => {
  const get = async (q: string) => {
    const res = await api.file(new Request(`http://x/api/file?${q}`));
    return { status: res.status, body: await res.json() };
  };
  test("returns the range with context", async () => {
    const { body } = await get("path=a/b.rs&start=10&end=12");
    expect(body.from).toBe(2);
    expect(body.lines.find((l: { n: number }) => l.n === 10).text).toBe("line 10");
  });
  test("refuses paths outside the fixture", async () => {
    expect((await get("path=../prompts.jsonl&start=1")).status).toBe(403);
    expect((await get("path=a/none.rs&start=1")).status).toBe(404);
  });
});
