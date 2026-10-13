// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Request handlers of the brief playground. Everything the handlers touch
// comes from `Config`, so the self-test points them at a fake binary.
import { existsSync } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, isAbsolute, join, resolve, sep } from "node:path";
import { BRIEF_CAP_BYTES, decisionOf, parseBrief, parseJsonRecord } from "./brief";
import { createStatus, type Status } from "./status";
import { abortCard, startRun, type Arm, type RunConfig } from "./codex";

export type Config = {
  /** Indexed checkout `pixel brief` runs in. */
  repo: string;
  /** Prompt corpus (JSONL). */
  promptsFile: string;
  /** User-added prompts (JSON array, gitignored). */
  localPromptsFile: string;
  /** Binary list (JSON array of {name, path}, gitignored). */
  binsFile: string;
  /** Binaries offered when `binsFile` is absent. */
  defaultBins: Bin[];
  timeoutMs: number;
  /** Codex runs (see codex.ts). */
  codex: RunConfig;
};

export type Bin = { name: string; path: string };
export type PromptRow = {
  id: string;
  text: string;
  lang: string;
  on_topic: boolean | null;
  kind: string | null;
  source: string | null;
  split: string | null;
  expected_files: string[];
  local: boolean;
};

const WORK_ROOT = "/private/tmp/claude-501/883-ab";
export const here = resolve(import.meta.dir, "..");

export const defaultConfig = (env: Record<string, string | undefined> = process.env): Config => {
  const repoRoot = resolve(here, "../..");
  const candidates: Bin[] = [
    { name: "installed pixel", path: Bun.which("pixel") ?? "pixel" },
    { name: "zero-pixel (PR #891)", path: `${WORK_ROOT}/zero-pixel` },
    { name: "new-pixel (PR #887)", path: `${WORK_ROOT}/new-pixel` },
  ];
  const fixture = `${WORK_ROOT}/zero/pixel`;
  const repo = env.PLAYGROUND_REPO ?? (existsSync(fixture) ? fixture : repoRoot);
  return {
    repo,
    codex: {
      // The real binary: the shell's `codex` is a wrapper function.
      codexBin: env.PLAYGROUND_CODEX ?? `${env.HOME}/.local/bin/codex`,
      root: env.PLAYGROUND_ROOT ?? "/private/tmp/claude-501/playground",
      userCodexHome: env.CODEX_HOME_USER ?? `${env.HOME}/.codex`,
      repo,
      inject: env.PLAYGROUND_INJECT === "prefix" ? "prefix" : "hook",
      runTimeoutMs: 300_000,
    },
    promptsFile: join(repoRoot, "eval/brief-gate/prompts.jsonl"),
    localPromptsFile: join(here, "local-prompts.json"),
    binsFile: join(here, "bins.json"),
    defaultBins: candidates.filter((b) => b.path === "pixel" || existsSync(b.path)),
    timeoutMs: 5000,
  };
};

const json = (body: unknown, status = 200) => Response.json(body, { status });
const fail = (error: string, status = 400) => json({ ok: false, error }, status);

const readJson = async <T>(path: string, fallback: T): Promise<T> => {
  try {
    return JSON.parse(await readFile(path, "utf8")) as T;
  } catch {
    return fallback;
  }
};

const asRow = (raw: Record<string, unknown>, local: boolean): PromptRow => ({
  id: String(raw.id),
  text: String(raw.text ?? ""),
  lang: typeof raw.lang === "string" ? raw.lang : "en",
  on_topic: typeof raw.on_topic === "boolean" ? raw.on_topic : null,
  kind: typeof raw.kind === "string" ? raw.kind : typeof raw.category === "string" ? raw.category : null,
  source: typeof raw.source === "string" ? raw.source : null,
  split: typeof raw.split === "string" ? raw.split : null,
  expected_files: Array.isArray(raw.expected_files) ? raw.expected_files.map(String) : [],
  local,
});

export const listPrompts = async (cfg: Config): Promise<PromptRow[]> => {
  const corpus = existsSync(cfg.promptsFile) ? (await readFile(cfg.promptsFile, "utf8")).split("\n") : [];
  const rows = corpus
    .filter((l) => l.trim())
    .flatMap((l) => {
      try {
        return [asRow(JSON.parse(l), false)];
      } catch {
        return [];
      }
    });
  const local = await readJson<Record<string, unknown>[]>(cfg.localPromptsFile, []);
  return [...rows, ...local.map((r) => asRow(r, true))];
};

export const addPrompt = async (cfg: Config, body: Record<string, unknown>): Promise<Response> => {
  const text = typeof body.text === "string" ? body.text.trim() : "";
  if (!text) return fail("text is required");
  const local = await readJson<Record<string, unknown>[]>(cfg.localPromptsFile, []);
  const next = Math.max(0, ...local.map((r) => Number(String(r.id).replace("local-", "")) || 0)) + 1;
  const row = {
    id: `local-${String(next).padStart(3, "0")}`,
    text,
    lang: typeof body.lang === "string" && body.lang ? body.lang : "en",
    on_topic: typeof body.on_topic === "boolean" ? body.on_topic : true,
    kind: typeof body.kind === "string" && body.kind ? body.kind : "local",
    source: "playground",
  };
  await mkdir(dirname(cfg.localPromptsFile), { recursive: true });
  await writeFile(cfg.localPromptsFile, `${JSON.stringify([...local, row], null, 2)}\n`);
  return json({ ok: true, prompt: asRow(row, true) });
};

type Spawned = { stdout: string; stderr: string; code: number | null; timedOut: boolean; ms: number };

const spawn = async (cmd: string[], cwd: string, timeoutMs: number): Promise<Spawned> => {
  const t0 = performance.now();
  try {
    const proc = Bun.spawn(cmd, { cwd, stdout: "pipe", stderr: "pipe", env: { ...process.env, NO_COLOR: "1" } });
    const done = Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
    const timeout = new Promise<"timeout">((r) => setTimeout(() => r("timeout"), timeoutMs));
    const out = await Promise.race([done, timeout]);
    if (out === "timeout") {
      // Do not wait for the pipes: a grandchild may keep them open.
      proc.kill();
      return { stdout: "", stderr: "", code: null, timedOut: true, ms: performance.now() - t0 };
    }
    return { stdout: out[0], stderr: out[1], code: out[2], timedOut: false, ms: performance.now() - t0 };
  } catch (e) {
    return { stdout: "", stderr: String(e), code: null, timedOut: false, ms: performance.now() - t0 };
  }
};

type BinInfo = Bin & { exists: boolean; json: boolean | null };

const configuredBins = async (cfg: Config): Promise<Bin[]> => {
  const configured = await readJson<Bin[] | null>(cfg.binsFile, null);
  return Array.isArray(configured) && configured.length ? configured : cfg.defaultBins;
};
const executable = (path: string) => (isAbsolute(path) ? path : (Bun.which(path) ?? path));

/** Binaries with their existence and whether `brief --help` mentions `--json`. */
export const listBins = async (cfg: Config): Promise<BinInfo[]> =>
  Promise.all(
    (await configuredBins(cfg)).map(async (b) => {
      const exe = executable(b.path);
      if (!existsSync(exe)) return { ...b, exists: false, json: null };
      const help = await spawn([exe, "brief", "--help"], cfg.repo, Math.min(3000, cfg.timeoutMs));
      return { ...b, exists: true, json: help.code === 0 ? help.stdout.includes("--json") : null };
    }),
  );

// Flag sets tried in order. A binary that rejects `--json` (or `--metrics`)
// exits non-zero with "unexpected argument"; the next set runs and the answer
// is flagged as plain text.
const FLAG_SETS: { flags: string[]; json: boolean }[] = [
  { flags: ["--json", "--metrics", "off"], json: true },
  { flags: ["--json"], json: true },
  { flags: ["--metrics", "off"], json: false },
  { flags: [], json: false },
];
const rejectsFlag = (r: Spawned) => r.code !== 0 && /unexpected argument|unrecognized|unknown (option|argument)/i.test(r.stderr);

const statusFor = (() => {
  const m = new WeakMap<Config, ReturnType<typeof createStatus>>();
  return (cfg: Config) => {
    let st = m.get(cfg);
    if (!st) m.set(cfg, (st = createStatus(cfg.repo, executable, spawn, Math.min(5000, cfg.timeoutMs * 4))));
    return st;
  };
})();

export const runBrief = async (cfg: Config, prompt: string, bin: string) => {
  if (!prompt.trim()) return fail("prompt is required");
  if (!bin) return fail("bin is required");
  if (!(await configuredBins(cfg)).some((b) => b.path === bin)) {
    return fail(`unknown binary: ${bin} (edit eval/playground/bins.json)`, 404);
  }
  const exe = executable(bin);
  if (!existsSync(exe)) return fail(`binary not found: ${bin}`, 404);
  if (!existsSync(cfg.repo)) return fail(`fixture repo not found: ${cfg.repo} (set PLAYGROUND_REPO, see README)`, 500);

  let last: Spawned | null = null;
  for (const set of FLAG_SETS) {
    const r = await spawn([exe, "brief", ...set.flags, prompt, "."], cfg.repo, cfg.timeoutMs);
    last = r;
    if (r.timedOut) return json({ ok: false, error: `timed out after ${cfg.timeoutMs} ms`, bin, latency_ms: Math.round(r.ms) }, 504);
    if (rejectsFlag(r)) continue;
    if (r.code !== 0) {
      return json({ ok: false, error: `exit ${r.code}: ${r.stderr.trim().slice(0, 500) || "(no stderr)"}`, bin, latency_ms: Math.round(r.ms) }, 502);
    }
    // The graph state at the moment the brief was built (cached for 10 s).
    const graph_status: Status = await statusFor(cfg).get(bin);
    const base = { ok: true, bin, repo: cfg.repo, latency_ms: Math.round(r.ms), cap_bytes: BRIEF_CAP_BYTES, graph_status };
    if (set.json) {
      const rec = parseJsonRecord(r.stdout);
      if (!rec) return json({ ok: false, error: "binary did not print a JSON object", raw: r.stdout.slice(0, 2000), bin }, 502);
      const brief = typeof rec.brief === "string" ? rec.brief : null;
      return json({
        ...base,
        json: true,
        brief,
        bytes: typeof rec.bytes === "number" ? rec.bytes : brief ? new TextEncoder().encode(brief).length : 0,
        parsed: brief ? parseBrief(brief) : null,
        decision: decisionOf(rec),
        record: rec,
      });
    }
    const text = r.stdout.replace(/\n+$/, "");
    return json({
      ...base,
      json: false,
      brief: text || null,
      bytes: new TextEncoder().encode(text).length,
      parsed: text ? parseBrief(text) : null,
      decision: null,
      record: null,
      note: "this binary has no `brief --json`: showing the raw text, no decision record",
    });
  }
  return json({ ok: false, error: `no flag set accepted: ${last?.stderr.trim().slice(0, 300)}`, bin }, 502);
};

// Read a cited range from the fixture so a chip can show the code.
export const readRange = async (cfg: Config, path: string, start: number, end: number) => {
  const root = resolve(cfg.repo);
  const full = resolve(root, path);
  if (!path || (full !== root && !full.startsWith(root + sep))) return fail("path escapes the fixture", 403);
  if (!existsSync(full)) return fail(`not in the fixture: ${path}`, 404);
  const lines = (await readFile(full, "utf8")).split("\n");
  const from = Math.max(1, Math.min(start, end) - 8);
  const to = Math.min(lines.length, Math.max(start, end) + 8);
  return json({
    ok: true,
    path,
    from,
    to,
    start,
    end,
    lines: lines.slice(from - 1, to).map((text, i) => ({ n: from + i, text })),
  });
};

export const runCard = async (cfg: Config, req: Request): Promise<Response> => {
  const b = (await req.json().catch(() => null)) as Record<string, unknown> | null;
  const cardId = typeof b?.cardId === "string" ? b.cardId : "";
  const prompt = typeof b?.prompt === "string" ? b.prompt.trim() : "";
  const arm = b?.arm;
  const briefText = typeof b?.briefText === "string" ? b.briefText : "";
  if (!cardId || !prompt) return fail("cardId and prompt are required");
  if (arm !== "pixel" && arm !== "none") return fail('arm must be "pixel" or "none"');
  if (arm === "pixel" && !briefText.trim()) return fail("the pixel arm needs briefText (Send first)");
  if (!existsSync(cfg.codex.repo)) return fail(`fixture repo not found: ${cfg.codex.repo}`, 500);
  return startRun({ ...cfg.codex, repo: cfg.repo }, { cardId, prompt, arm: arm as Arm, briefText });
};

export const handlers = (cfg: Config) => ({
  status: async (req: Request) => {
    const bin = new URL(req.url).searchParams.get("bin") ?? "";
    if (!(await configuredBins(cfg)).some((b) => b.path === bin)) return fail(`unknown binary: ${bin}`, 404);
    if (!existsSync(executable(bin))) return fail(`binary not found: ${bin}`, 404);
    const st = statusFor(cfg);
    return json({ ok: true, ...(await st.get(bin)), refresh: st.refreshState() });
  },
  refresh: async (req: Request) => {
    const b = (await req.json().catch(() => null)) as { bin?: unknown } | null;
    const bin = typeof b?.bin === "string" ? b.bin : "";
    if (!(await configuredBins(cfg)).some((x) => x.path === bin)) return fail(`unknown binary: ${bin}`, 404);
    if (!existsSync(executable(bin))) return fail(`binary not found: ${bin}`, 404);
    return json({ ok: true, ...statusFor(cfg).startRefresh(bin) });
  },
  run: (req: Request) => runCard(cfg, req),
  abort: async (req: Request) => {
    const b = (await req.json().catch(() => null)) as { cardId?: unknown } | null;
    if (typeof b?.cardId !== "string") return fail("cardId is required");
    return json({ ok: true, killed: abortCard(b.cardId) });
  },
  prompts: async () => json(await listPrompts(cfg)),
  addPrompt: async (req: Request) => {
    const body = await req.json().catch(() => null);
    return body && typeof body === "object" ? addPrompt(cfg, body as Record<string, unknown>) : fail("JSON body required");
  },
  bins: async () => json({ repo: cfg.repo, bins: await listBins(cfg) }),
  brief: async (req: Request) => {
    const body = (await req.json().catch(() => null)) as { prompt?: unknown; bin?: unknown } | null;
    return runBrief(cfg, String(body?.prompt ?? ""), String(body?.bin ?? ""));
  },
  file: async (req: Request) => {
    const u = new URL(req.url);
    const start = Number(u.searchParams.get("start") ?? 1);
    const end = Number(u.searchParams.get("end") ?? start);
    return readRange(cfg, u.searchParams.get("path") ?? "", start, end);
  },
});
