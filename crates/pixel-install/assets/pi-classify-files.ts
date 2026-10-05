/**
 * pixel-classify-files — the ten-levels "Jev files" tools (levels 8 + 9) rebuilt on `pixel classify`.
 *
 * Registered tools:
 *   ask_pixel_file_bool(path, question, yes?, no?)     noul over one file
 *   ask_pixel_file_choice(path, question, options)     choice over one file
 *   ask_pixel_file_score(path, question, levels)       score over one file
 *   ask_pixel_files(paths_or_globs, questions_json)    one decision block per file, in parallel
 *   pick_pixel_file(question, candidates)              which file to open first
 *
 * Every decision is a `pixel classify` subprocess: state text = "path: <p>\n\ncontent:\n<file>",
 * context = the question, labels + criteria = the bounded answer set. The engine is whatever
 * `pixel config classify-engine` resolves (local Ollaya /v1/systemone when warm, remote preset
 * otherwise) — no Jev/OpenRouter key is needed on top of the pixel config. The file's text never
 * enters the agent's context; only typed answers come back.
 *
 * Requires `pixel config classify on`.
 */
import { execFile } from "node:child_process";
import { glob, readFile, realpath, stat } from "node:fs/promises";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { Type } from "typebox";

const PIXEL = process.env.PIXEL_BIN ?? "pixel";
const CALL_TIMEOUT_MS = 90_000;
const MAX_FILE_CHARS = 14_000; // local Ollaya engine 422s above ~16KB of state; keep headroom
const MAX_FILES = 255;
const CONCURRENCY = 8;
const SKIP_DIRS = new Set(["node_modules", ".git", ".sessions", "dist", "build", "coverage", ".pi"]);
const BINARY_EXT = /\.(png|jpe?g|gif|webp|ico|pdf|zip|gz|tgz|woff2?|ttf|mp[34]|mov|lock)$/i;
const GLOB_CHARS = /[*?[\]{}]/;

const ok = (payload: unknown) => ({ content: [{ type: "text", text: JSON.stringify(payload, null, 2) }], details: payload });
const fail = (err: any) => ({ content: [{ type: "text", text: `error: ${err?.message ?? err}` }], isError: true });

class FileStateError extends Error {
  readonly path: string;
  constructor(message: string, path: string) {
    super(message);
    this.name = "FileStateError";
    this.path = path;
  }
}

// ---------- file state (ported from level08/read-state.ts) ----------

const looksBinary = (buf: Buffer) => buf.subarray(0, 8192).includes(0);

async function readFileState(path: string, cwd: string): Promise<{ path: string; content: string }> {
  const full = isAbsolute(path) ? path : resolve(cwd, path);
  let info;
  try {
    info = await stat(full);
  } catch {
    throw new FileStateError(`not found: ${path}`, path);
  }
  if (!info.isFile()) throw new FileStateError(`not a file: ${path}`, path);
  // The file's contents can reach a hosted classifier, so reads are confined
  // to the session root: realpath both sides so an absolute path or a
  // symlink escaping `cwd` is refused rather than shipped.
  const [root, resolved] = await Promise.all([realpath(cwd), realpath(full)]);
  if (resolved !== root && !resolved.startsWith(root + sep)) {
    throw new FileStateError(`outside the session root: ${path}`, path);
  }
  if (info.size > MAX_FILE_CHARS) {
    throw new FileStateError(`too large for one classify call: ${path} is ${info.size} bytes, the limit is ${MAX_FILE_CHARS}`, path);
  }
  const buf = await readFile(resolved);
  if (looksBinary(buf)) throw new FileStateError(`binary: ${path}`, path);
  return { path, content: buf.toString("utf8") };
}

// ---------- glob expand + prune (ported from level09/prune.ts) ----------

export interface Skipped {
  path: string;
  reason: string;
}

async function expandPatterns(patterns: string[], cwd: string, recursive: boolean): Promise<string[]> {
  const out = new Set<string>();
  for (const raw of patterns) {
    const pattern = raw.trim();
    if (!pattern) continue;
    if (GLOB_CHARS.test(pattern)) {
      for await (const p of glob(pattern, { cwd })) out.add(String(p));
      continue;
    }
    const full = isAbsolute(pattern) ? pattern : resolve(cwd, pattern);
    let info;
    try {
      info = await stat(full);
    } catch {
      out.add(pattern);
      continue;
    }
    if (info.isFile()) {
      out.add(pattern);
      continue;
    }
    for await (const p of glob(recursive ? `${pattern.replace(/\/+$/, "")}/**/*` : `${pattern.replace(/\/+$/, "")}/*`, { cwd })) out.add(String(p));
  }
  return [...out].sort();
}

async function pruneFiles(paths: string[], cwd: string, cap = MAX_FILES): Promise<{ files: string[]; skipped: Skipped[] }> {
  const files: string[] = [];
  const skipped: Skipped[] = [];
  for (const path of paths) {
    const full = isAbsolute(path) ? path : resolve(cwd, path);
    const rel = relative(cwd, full);
    if (rel.startsWith("..")) { skipped.push({ path, reason: "outside the repo" }); continue; }
    if (rel.split(sep).some((part) => SKIP_DIRS.has(part))) { skipped.push({ path, reason: "skipped directory" }); continue; }
    let info;
    try {
      info = await stat(full);
    } catch {
      skipped.push({ path, reason: "not found" });
      continue;
    }
    if (!info.isFile()) continue;
    if (info.size === 0) { skipped.push({ path, reason: "empty" }); continue; }
    if (info.size > MAX_FILE_CHARS) { skipped.push({ path, reason: `too large, ${info.size} bytes` }); continue; }
    if (BINARY_EXT.test(path)) { skipped.push({ path, reason: "binary or lock file" }); continue; }
    if (files.length >= cap) { skipped.push({ path, reason: `over the ${cap} file cap; narrow the pattern` }); continue; }
    files.push(path);
  }
  return { files, skipped };
}

// ---------- pixel classify transport ----------

interface ClassifyResult {
  predicted: string;
  probs: Record<string, number>;
  confidence: number;
  model?: string;
  provider?: string;
  ms: number;
}

/** One `pixel classify` call. Throws with stderr text on failure. */
function pixelClassify(stateText: string, context: string, criteria: Record<string, string | null>): Promise<ClassifyResult> {
  const args = ["classify", stateText, "--context", context, "--json", "--metrics", "off"];
  for (const label of Object.keys(criteria)) args.push("--label", label);
  for (const [label, criterion] of Object.entries(criteria)) {
    if (criterion) args.push("--criterion", `${label}=${criterion}`);
  }
  const started = performance.now();
  return new Promise((resolveP, reject) => {
    execFile(PIXEL, args, { timeout: CALL_TIMEOUT_MS, maxBuffer: 4 * 1024 * 1024 }, (err, stdout, stderr) => {
      if (err) return reject(new Error(`pixel classify failed: ${(stderr || err.message).trim().slice(0, 500)}`));
      let parsed: any;
      try {
        parsed = JSON.parse(stdout);
      } catch {
        return reject(new Error(`pixel classify returned non-JSON: ${stdout.slice(0, 300)}`));
      }
      if (parsed.ok === false || parsed.error) return reject(new Error(`pixel classify: ${parsed.error?.message ?? "error"}`));
      resolveP({
        predicted: parsed.predicted,
        probs: parsed.probs ?? {},
        confidence: parsed.snapshot?.confidence ?? 0,
        model: parsed.snapshot?.model,
        provider: parsed.snapshot?.provider,
        ms: Math.round(performance.now() - started),
      });
    });
  });
}

/** Run `fn` over `items` with at most `limit` in flight. Results keep input order. */
async function parallel<T, R>(items: T[], limit: number, fn: (item: T) => Promise<R>): Promise<R[]> {
  const out: R[] = new Array(items.length);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const i = next++;
      out[i] = await fn(items[i]);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return out;
}

// ---------- Jev question blocks -> pixel classify ----------

const instructionsText = (i: unknown): string => (typeof i === "string" ? i : JSON.stringify(i));

function validateQuestions(questions: unknown): asserts questions is Record<string, any> {
  if (!questions || typeof questions !== "object" || Array.isArray(questions) || Object.keys(questions).length === 0) {
    throw new Error("questions_json must be a nonempty object keyed by question id");
  }
  for (const [id, q] of Object.entries(questions as Record<string, any>)) {
    if (!q || typeof q !== "object") throw new Error(`question "${id}" must be an object`);
    if (q.type !== "noul" && q.type !== "choice" && q.type !== "score") throw new Error(`question "${id}" has a missing or unknown type`);
    if (typeof q.instructions === "string" ? !q.instructions.trim() : typeof q.instructions !== "object") {
      throw new Error(`question "${id}" needs nonblank string or object instructions`);
    }
    if (q.type === "choice") {
      const options = Object.keys(q.criteria ?? {});
      if (!options.length) throw new Error(`choice "${id}" has no options`);
      if (options.length > MAX_FILES) throw new Error(`choice "${id}" has ${options.length} options; the maximum is ${MAX_FILES}`);
    }
    if (q.type === "score") {
      if (!Array.isArray(q.criteria) || q.criteria.length < 2 || q.criteria.length > 10) {
        throw new Error(`score "${id}" needs 2 to 10 level descriptions`);
      }
    }
  }
}

function parseQuestions(questionsJson: string): Record<string, any> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(questionsJson);
  } catch (err: any) {
    throw new Error(`questions_json is not valid JSON: ${err.message}`);
  }
  validateQuestions(parsed);
  return parsed as Record<string, any>;
}

interface Answer {
  type: "noul" | "choice" | "score";
  noul?: number;
  choice?: string;
  score?: number;
  legend?: Record<string, string>;
  probabilities?: Record<string, number>;
  confidence?: number;
}

/** Answer one question of a block about one state. One classify call per question. */
async function answerQuestion(stateText: string, q: any): Promise<Answer> {
  const context = instructionsText(q.instructions);
  if (q.type === "noul") {
    const criteria: Record<string, string | null> = { yes: q.criteria?.true ?? null, no: q.criteria?.false ?? null };
    const r = await pixelClassify(stateText, `${context} Answer yes or no.`, criteria);
    return { type: "noul", noul: r.probs.yes ?? (r.predicted === "yes" ? 1 : 0) };
  }
  if (q.type === "choice") {
    const r = await pixelClassify(stateText, context, q.criteria);
    return { type: "choice", choice: r.predicted, probabilities: r.probs, confidence: r.confidence };
  }
  // score: ordered levels become labels; the answer is the probability-weighted position
  const levels: string[] = q.criteria;
  const criteria: Record<string, string | null> = {};
  const legend: Record<string, string> = {};
  levels.forEach((desc, i) => {
    criteria[`level_${i + 1}`] = desc;
    legend[String(i + 1)] = desc;
  });
  const r = await pixelClassify(stateText, `${context} Pick the level that best fits, lowest to highest.`, criteria);
  let score = 0;
  const probabilities: Record<string, number> = {};
  levels.forEach((_d, i) => {
    const p = r.probs[`level_${i + 1}`] ?? 0;
    probabilities[String(i + 1)] = p;
    score += p * (i + 1);
  });
  return { type: "score", score: Math.round(score * 1000) / 1000, legend, probabilities, confidence: r.confidence };
}

async function decideFile(pi: any, source: string, file: { path: string; content: string }, questions: Record<string, any>): Promise<Record<string, Answer>> {
  const stateText = `path: ${file.path}\n\ncontent:\n${file.content}`;
  const entries = await parallel(Object.entries(questions), 4, async ([id, q]) => [id, await answerQuestion(stateText, q)] as const);
  const answers = Object.fromEntries(entries);
  try {
    process.stderr.write("PIXEL_CLASSIFY_EVENT " + JSON.stringify({ kind: "classify", source, path: file.path, at: Date.now() }) + "\n");
    pi.appendEntry?.("pixel-classify", { source, path: file.path, questions: Object.keys(questions) });
  } catch { /* side channel is best effort */ }
  return answers;
}

const QUESTION_SCHEMA =
  'questions_json is a JSON object keyed by question id. Three types. ' +
  'noul: {"type":"noul","instructions":"Does `content` ...?","criteria":{"true":"...","false":"..."}} returns a probability of yes. ' +
  'choice: {"type":"choice","instructions":"Which ... is `content`?","criteria":{"option_a":"when it applies","option_b":"...","other":"none of the above"}} returns one of your keys plus confidence, up to 255 options. ' +
  'score: {"type":"score","instructions":"How ... is `content`?","criteria":["lowest situation","...","highest situation"]} returns a position on your levels, two to ten of them. ' +
  "Write every question against `content`, the file's text; `path` is also in the state. Ask every question you might need in one block; each question is one classify call per file.";

const WHEN =
  "Use this for a judgment about what a file does or contains, without reading it into your context. " +
  "Write the question against `content`, which is the file's text. Use the read tool instead when you need the code itself, to edit or quote it. " +
  "Exact lookups, does this string appear, how many lines, belong to grep, not here.";

// ---------- pi registration ----------

export default function (pi: any) {
  pi.registerTool({
    name: "ask_pixel_file_bool",
    label: "Ask pixel classify about a file, yes or no",
    description: `Yes or no about one file. Returns { path, answer, noul } where noul is the probability of yes, 0 to 1. ${WHEN}`,
    parameters: Type.Object({
      path: Type.String({ description: "File path, relative to the repo" }),
      question: Type.String({ description: "A yes or no question about `content`, for example: Does `content` validate authentication tokens?" }),
      yes: Type.Optional(Type.String({ description: "What counts as yes" })),
      no: Type.Optional(Type.String({ description: "What counts as no" })),
    }),
    async execute(_id: string, p: any, _signal: AbortSignal, _u: any, ctx: any) {
      try {
        const file = await readFileState(p.path, ctx.cwd);
        const q: any = { type: "noul", instructions: p.question };
        if (p.yes !== undefined || p.no !== undefined) q.criteria = { true: p.yes, false: p.no };
        const answers = await decideFile(pi, "ask_pixel_file_bool", file, { ask: q });
        const a = answers.ask;
        return ok({ path: file.path, answer: (a.noul ?? 0) > 0.5, noul: a.noul });
      } catch (err) { return fail(err); }
    },
  });

  pi.registerTool({
    name: "ask_pixel_file_choice",
    label: "Ask pixel classify about a file, pick one",
    description: `Pick one option about one file. Returns { path, choice, confidence, probabilities }. The choice is always one of your options; an "other" option is added if you leave none. ${WHEN}`,
    parameters: Type.Object({
      path: Type.String({ description: "File path, relative to the repo" }),
      question: Type.String({ description: "The question, for example: Which layer is `content`?" }),
      options: Type.Record(Type.String(), Type.String(), { description: "Option name to a one line description of when it applies. Up to 255." }),
    }),
    async execute(_id: string, p: any, _signal: AbortSignal, _u: any, ctx: any) {
      try {
        const file = await readFileState(p.path, ctx.cwd);
        const options = { ...p.options };
        const hasExit = Object.keys(options).some((k) => /^(other|none|unknown)$/i.test(k));
        if (!hasExit) options.other = "none of the above applies";
        const answers = await decideFile(pi, "ask_pixel_file_choice", file, { ask: { type: "choice", instructions: p.question, criteria: options } });
        const a = answers.ask;
        return ok({ path: file.path, choice: a.choice, confidence: a.confidence, probabilities: a.probabilities });
      } catch (err) { return fail(err); }
    },
  });

  pi.registerTool({
    name: "ask_pixel_file_score",
    label: "Ask pixel classify about a file, on a scale",
    description: `A position on a scale you define, about one file. Returns { path, score, top, nearest, confidence, legend }. Levels are ordered low to high, two to ten of them, each a described situation. ${WHEN}`,
    parameters: Type.Object({
      path: Type.String({ description: "File path, relative to the repo" }),
      question: Type.String({ description: "The question, for example: How risky is a refactor of `content`?" }),
      levels: Type.Array(Type.String(), { description: "Ordered low to high, each level a situation, for example: Isolated and well tested" }),
    }),
    async execute(_id: string, p: any, _signal: AbortSignal, _u: any, ctx: any) {
      try {
        const file = await readFileState(p.path, ctx.cwd);
        const answers = await decideFile(pi, "ask_pixel_file_score", file, { ask: { type: "score", instructions: p.question, criteria: p.levels } });
        const a = answers.ask;
        const nearest = a.probabilities ? Object.entries(a.probabilities).sort((x, y) => y[1] - x[1])[0]?.[0] : undefined;
        return ok({ path: file.path, score: a.score, top: nearest ? Number(nearest) : undefined, nearest: nearest ? a.legend?.[nearest] : undefined, confidence: a.confidence, legend: a.legend });
      } catch (err) { return fail(err); }
    },
  });

  pi.registerTool({
    name: "ask_pixel_files",
    label: "Ask pixel classify about many files",
    description:
      "Ask the same typed questions of many files at once without reading any of them. Code expands globs and directories, " +
      "drops node_modules, .git, binaries, and files over the budget, caps the list at 255, then makes one pixel classify call per question per file in parallel. " +
      "Returns { results: [{ path, answers }], skipped: [{ path, reason }], calls }. " + QUESTION_SCHEMA +
      " Use read when you need a file's code; use grep for exact strings.",
    parameters: Type.Object({
      paths_or_globs: Type.Array(Type.String(), { description: 'Files, directories, or globs, relative to the repo, for example ["src/**/*.ts"] or ["src/http"]' }),
      questions_json: Type.String({ description: "The question block as a JSON string" }),
      recursive: Type.Optional(Type.Boolean({ description: "For directories: include every file below them. Default false." })),
    }),
    async execute(_id: string, p: any, _signal: AbortSignal, _u: any, ctx: any) {
      try {
        const questions = parseQuestions(p.questions_json);
        const expanded = await expandPatterns(p.paths_or_globs, ctx.cwd, p.recursive ?? false);
        const { files, skipped } = await pruneFiles(expanded, ctx.cwd);
        const results: { path: string; answers: Record<string, Answer> }[] = [];
        let calls = 0;
        await parallel(files, CONCURRENCY, async (path) => {
          try {
            const file = await readFileState(path, ctx.cwd);
            const answers = await decideFile(pi, "ask_pixel_files", file, questions);
            results.push({ path, answers });
            calls += Object.keys(questions).length;
          } catch (err: any) {
            skipped.push({ path, reason: err instanceof FileStateError ? err.message : `call failed: ${err?.message ?? err}` });
          }
        });
        results.sort((a, b) => a.path.localeCompare(b.path));
        return ok({ results, skipped, calls });
      } catch (err) { return fail(err); }
    },
  });

  pi.registerTool({
    name: "pick_pixel_file",
    label: "Pick the file to open first",
    description:
      "After ask_pixel_files, choose which of a list of files to open first for a goal. One choice keyed by path, so the pick is always a real file. " +
      "Returns { path | null, confidence, probabilities }. Pass a short note per path if you have one, for example the answers you already got.",
    parameters: Type.Object({
      question: Type.String({ description: "The goal, for example: Which file should I open first to fix the proration bug?" }),
      candidates: Type.Array(Type.Object({ path: Type.String(), note: Type.Optional(Type.String()) }), { description: "Paths, with an optional one line note each" }),
    }),
    async execute(_id: string, p: any, _signal: AbortSignal) {
      try {
        if (!p.candidates.length) return ok({ path: null, confidence: 0, probabilities: {} });
        const shown = p.candidates.slice(0, MAX_FILES - 1);
        const criteria: Record<string, string | null> = {};
        for (const c of shown) criteria[c.path] = c.note ?? null;
        criteria.none = "No file in the list fits";
        const stateText = p.question + "\n\nfiles:\n" + shown.map((c: any) => c.path).join("\n");
        const r = await pixelClassify(stateText, p.question, criteria);
        const path = r.predicted === "none" || r.confidence < 0.3 ? null : r.predicted;
        return ok({ path, confidence: r.confidence, probabilities: r.probs });
      } catch (err) { return fail(err); }
    },
  });
}
