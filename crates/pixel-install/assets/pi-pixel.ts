// Pixel extension for Pi — managed by `pixel install --repo`.
// __MANAGED_BEGIN__
// __MANAGED_END__
import { spawn, spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, realpathSync, statSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { Type } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

const PIXEL_BIN = __PIXEL_BIN__;
const MAX_OUTPUT = 16000;
const READ_LIMIT = 200;
const BOOTSTRAP_BUDGET = 2400;
const MIN_PROMPT_LEN = 12;
const CREDENTIAL_PATH = /(?:^|\/)(?:\.env(?:\.[^/]*)?|\.ssh|\.aws|\.npmrc|\.netrc|id_(?:rsa|ed25519|ecdsa|dsa)[^/]*|[^/]*\.(?:pem|key|p12|pfx)|[^/]*(?:credentials|secrets?)[^/]*)(?:\/|$)/i;
// The task-intent verdict is a best-effort extra of the bootstrap: a warm
// local classifier answers in about 0.1 s, a cold one takes seconds and is
// dropped. Under one half, the label is not more likely than the others.
const INTENT_TIMEOUT_MS = 500;
const MIN_INTENT_P = 0.5;
const EDIT_TOOLS = new Set(["edit", "write", "apply_patch", "edit_file", "replace_file_content", "write_to_file"]);
const ACTIONS = ["scope_task", "list_areas", "search_content", "find_code", "impact", "pack_context", "what_changed", "review_changes", "fetch", "commit", "commit_and_push"] as const;
type Action = (typeof ACTIONS)[number];
type Policy = "advisory" | "enforce" | "off";
const POLICIES = new Set<string>(["advisory", "enforce", "off"]);
const policies = new Map<string, Policy>();

// The environment wins without a spawn; otherwise the layered configuration
// decides (`pixel config policy`, the repository file over the global one),
// read once per project root. A Pixel that cannot answer leaves advisory in
// place: configuration is best effort and native tools stay available.
function policyFor(root: string): Policy {
  if (["0", "false", "off"].includes(process.env.PIXEL_TARGETS_GUARD?.trim().toLowerCase() ?? "")) return "off";
  const override = process.env.PIXEL_POLICY?.trim().toLowerCase();
  if (override) return POLICIES.has(override) ? override as Policy : "advisory";
  const cached = policies.get(root);
  if (cached) return cached;
  let policy: Policy = "advisory";
  try {
    const result = spawnSync(PIXEL_BIN, ["config", "policy", "--json", "--metrics", "off"], { cwd: root, encoding: "utf8", timeout: 5000 });
    const reported = result.status === 0 ? JSON.parse(String(result.stdout)).policy : undefined;
    if (POLICIES.has(reported)) policy = reported;
  } catch { /* Unreadable configuration keeps the advisory default. */ }
  policies.set(root, policy);
  return policy;
}
const LEGACY_OP: Record<string, string> = {
  "scope-task": "targets", "list-areas": "clusters", "search-content": "search",
  "find-code": "resolve", "pack-context": "context", "what-changed": "changes",
  "review-changes": "review", "fetch": "sync", "commit": "publish",
  "commit-and-push": "ship", "repo-state": "inspect", "commit-history": "history",
  "who-wrote": "provenance",
};

let installed: { version: string; commands: Set<string> } | undefined;
function capabilities() {
  if (installed) return installed;
  const version = spawnSync(PIXEL_BIN, ["--version"], { encoding: "utf8", timeout: 5000 });
  const help = spawnSync(PIXEL_BIN, ["--help"], { encoding: "utf8", timeout: 5000 });
  if (version.status !== 0 || help.status !== 0) {
    throw new Error(`Pixel unavailable at ${PIXEL_BIN}: ${version.error?.message ?? help.error?.message ?? help.stderr?.trim() ?? "failed preflight"}`);
  }
  installed = { version: version.stdout.trim().split("\n")[0], commands: new Set(
    [...help.stdout.matchAll(/^  ([a-z][a-z-]+)\s{2,}/gm)].map((match) => match[1]),
  ) };
  return installed;
}

// Tool-facing runs keep Pixel's metrics box so the user sees what Pixel
// saved; `quiet` is for the extension's own probes (health, bootstrap, the
// post-edit snapshot), which must not add boxes or count as usage.
function run(root: string, args: string[], quiet = false) {
  return runBox(root, args, quiet).stdout;
}

// Pixel prints its metrics box on stderr; only the box lines (from the
// "🟩 pixel" header to the closing "└" rule) are kept, never diagnostics.
function metricsBox(stderr: string) {
  const lines = String(stderr ?? "").split("\n");
  const start = lines.findIndex((line) => line.includes("🟩 pixel"));
  if (start < 0) return "";
  const rest = lines.slice(start);
  const closing = rest.findIndex((line) => line.trimStart().startsWith("└"));
  const blank = rest.findIndex((line) => !line.trim());
  const end = closing >= 0 ? closing + 1 : blank >= 0 ? blank : rest.length;
  const box = rest.slice(0, end);
  return box.join("\n").trim();
}

function runBox(root: string, args: string[], quiet = false) {
  const operation = resolveOperation(args[0]);
  const result = spawnSync(PIXEL_BIN, [operation, ...args.slice(1), ...(quiet ? ["--metrics", "off"] : [])], {
    cwd: root, encoding: "utf8", timeout: 15000, maxBuffer: 2_000_000,
  });
  if (result.error || result.status !== 0) {
    throw new Error(result.error?.message ?? result.stderr?.trim() ?? `Pixel exited ${result.status}`);
  }
  return { stdout: result.stdout as string, box: quiet ? "" : metricsBox(result.stderr) };
}

// The asynchronous twin of `run`, so the bootstrap's calls share the wait.
function runAsync(root: string, args: string[]): Promise<string> {
  const operation = resolveOperation(args[0]);
  return collect(root, [operation, ...args.slice(1), "--metrics", "off"], 15000)
    .then(({ code, stdout, stderr }) => {
      if (code !== 0) throw new Error(stderr.trim() || `Pixel exited ${code}`);
      return stdout;
    });
}

// Spawn Pixel and gather its output; past `timeout` the child is killed and
// the call settles with code null.
function collect(root: string, args: string[], timeout: number, input?: string, signal?: AbortSignal): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((done) => {
    if (signal?.aborted) { done({ code: null, stdout: "", stderr: "cancelled" }); return; }
    let stdout = "", stderr = "", settled = false;
    const finish = (code: number | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      signal?.removeEventListener("abort", abort);
      done({ code, stdout, stderr });
    };
    const child = spawn(PIXEL_BIN, args, { cwd: root, stdio: [input === undefined ? "ignore" : "pipe", "pipe", "pipe"] });
    const abort = () => { child.kill("SIGKILL"); finish(null); };
    const timer = setTimeout(() => { child.kill("SIGKILL"); stderr ||= `Pixel timed out after ${timeout} ms`; finish(null); }, timeout);
    signal?.addEventListener("abort", abort, { once: true });
    child.stdin?.on("error", () => finish(null));
    child.stdin?.end(input);
    child.stdout?.setEncoding("utf8").on("data", (chunk: string) => { stdout += chunk; });
    child.stderr?.setEncoding("utf8").on("data", (chunk: string) => { stderr += chunk; });
    child.on("error", (error) => { stderr ||= error.message; finish(null); });
    child.on("close", (code) => finish(code));
  });
}

const TASK_UNAVAILABLE = "Pixel task state is unavailable. Reads and recovery remain available; edits and completion require a working task ledger.";
type TaskBinding = { task_id: string; attempt_id: string; session_id?: string; branch_id?: string };
type TaskDecision = { decision: "allow" | "deny" | "continue" | "observe"; reason?: string; context?: string; task_id?: string; attempt_id?: string };
const taskOwners: Map<string, symbol> = ((globalThis as any)[Symbol.for("pixel.task.owners")] ??= new Map());

const readFlags = (args: string[], allowed: string[], prefixes: string[]): boolean => {
  const end = args.indexOf("--");
  return args.slice(0, end < 0 ? args.length : end).every((arg) => !arg.startsWith("-") || allowed.includes(arg) || prefixes.some((prefix) => arg.startsWith(prefix)));
};
const gitRead = (args: string[]): boolean => ["status", "diff", "log", "show", "rev-parse", "ls-files"].includes(args[0]) && readFlags(args.slice(1), [
  "-h", "--help", "-s", "--short", "-b", "--branch", "--porcelain", "--porcelain=v1", "--porcelain=v2",
  "-z", "-v", "-t", "-m", "-o", "-d", "-c", "-u", "--cached", "--staged", "--others", "--exclude-standard",
  "--modified", "--deleted", "--unmerged", "--stage", "--check", "--stat", "--numstat", "--shortstat",
  "--name-only", "--name-status", "--summary", "--patch", "-p", "--no-patch", "--binary", "--raw",
  "--exit-code", "--quiet", "--no-ext-diff", "--no-textconv", "--no-renames", "--no-index",
  "--oneline", "--graph", "--decorate", "--all", "--no-decorate", "--no-color", "--reverse",
  "--show-toplevel", "--show-prefix", "--git-dir", "--git-path", "--absolute-git-dir", "--verify",
  "--revs-only", "--is-inside-work-tree", "--abbrev-ref", "--symbolic-full-name", "--end-of-options",
], ["--format=", "--pretty=", "--max-count=", "--since=", "--until=", "--color=", "--unified="]);
const searchRead = (args: string[]): boolean => readFlags(args, [
  "-h", "--help", "--version", "-n", "-i", "-v", "-l", "-L", "-c", "-q", "-w", "-x", "-s",
  "-F", "-E", "-e", "-f", "-g", "-r", "-R", "-H", "-I", "-a", "-o", "-U", "-z", "-0",
  "-A", "-B", "-C", "-m", "--files", "--hidden", "--no-ignore", "--no-config", "--json",
  "--line-number", "--ignore-case", "--fixed-strings", "--count", "--files-with-matches",
  "--files-without-match", "--glob", "--iglob", "--type", "--type-not", "--max-count",
  "--context", "--after-context", "--before-context", "--regexp", "--file", "--no-heading", "--heading",
], ["--glob=", "--iglob=", "--type=", "--type-not=", "--max-count=", "--context=", "--after-context=", "--before-context=", "--regexp=", "--file=", "--color="]);
const pixelReadOrRecovery = (args: string[]): boolean => {
  if (args[0] === "config") return args.length === 1 || args.length === 2 && ["policy", "metrics", "--help", "-h"].includes(args[1]);
  if (["task", "task-state"].includes(args[0])) return ["begin", "contract", "prepare", "verify", "review", "finish", "route", "cancel", "recover", "status", "events", "replay", "--help", "-h"].includes(args[1]);
  return ["status", "doctor", "build-index", "scope-task", "execution-brief", "find-code", "find-symbol", "search-content", "search-meaning", "impact", "pack-context", "what-changed", "review-changes", "repo-state", "list-areas", "list-flows", "who-calls", "capabilities", "--help", "--version"].includes(args[0]);
};
const sortRead = (args: string[]): boolean => readFlags(args, ["-r", "-n", "-u", "-f", "-b", "-d", "-g", "-h", "-M", "-V", "-s", "-z", "-c", "-C", "--reverse", "--numeric-sort", "--unique", "--ignore-case", "--stable", "--check", "--zero-terminated"], ["--key=", "--field-separator="]);
const uniqRead = (args: string[]): boolean => {
  const separator = args.indexOf("--");
  const end = separator < 0 ? args.length : separator;
  const operands = args.slice(0, end).filter((arg) => !arg.startsWith("-")).length + Math.max(0, args.length - end - 1);
  return operands <= 1 && readFlags(args, ["-c", "-d", "-u", "-i", "-z", "--count", "--repeated", "--unique", "--ignore-case", "--zero-terminated"], ["--skip-fields=", "--skip-chars=", "--check-chars="]);
};

// Only syntactically simple reads/recovery survive a missing task engine.
const mayMutate = (tool: string, input: any): boolean => {
  if (EDIT_TOOLS.has(tool)) return true;
  if (tool === "pixel" || tool === "pixel_project") return !["scope_task", "list_areas", "search_content", "find_code", "impact", "pack_context", "what_changed", "review_changes"].includes(input?.action);
  if (!["Bash", "bash", "shell", "local_shell", "unified_exec", "exec_command"].includes(tool)) return !["Read", "read", "Glob", "glob", "Grep", "grep", "WebSearch", "web_search", "WebFetch", "web_fetch", "AskUserQuestion", "ls", "find", "list_dir", "grep_search", "file_search", "view_file"].includes(tool);
  const command = String(input?.command ?? input?.cmd ?? "");
  const segments = splitShellSegments(command);
  if (!segments || segments.slice(1).some((segment) => !segment.piped)) return true;
  return segments.some((segment) => taskLeafMutates(segment.text, segments.length === 1));
};
const taskLeafMutates = (command: string, recovery: boolean): boolean => {
  let words = tokenizeShell(command, true);
  if (!words?.length) return true;
  const base = (name: string) => name.split("/").at(-1);
  if (base(words[0]) === "rtk") words = words.slice(words[1] === "proxy" ? 2 : 1);
  if (["pixel", "pixel-dev"].includes(base(words[0] ?? "") ?? "")) return !pixelReadOrRecovery(words.slice(1)) || !recovery && ["task", "task-state", "doctor", "build-index"].includes(words[1]);
  if (base(words[0] ?? "") === "git") return !gitRead(words.slice(1));
  if (["rg", "grep"].includes(base(words[0] ?? "") ?? "")) return !searchRead(words.slice(1));
  if (base(words[0] ?? "") === "sort") return !sortRead(words.slice(1));
  if (base(words[0] ?? "") === "uniq") return !uniqRead(words.slice(1));
  return !["pwd", "true", "false", "cat", "head", "tail", "wc", "ls", "read"].includes(base(words[0] ?? "") ?? "");
};

// Authority, completion budgets and deduplication live in the task engine.
const taskEvent = async (ctx: ExtensionContext, event: string, payload: Record<string, unknown>, branch: string | undefined, binding: TaskBinding | undefined, branchUnbound: boolean): Promise<TaskDecision> => {
  try {
    const session = ctx.sessionManager.getSessionId();
    const input = { ...payload, cwd: ctx.cwd, session_id: session, branch_id: branch, task_id: binding?.task_id, attempt_id: binding?.attempt_id, binding_session_id: binding?.session_id, branch_unbound: branchUnbound };
    const result = await collect(ctx.cwd, ["run-hook", "task-event", "--provider", "pi", "--event", event, "--metrics", "off"], 8000, JSON.stringify(input), event === "interrupt" || event === "session-end" ? undefined : ctx.signal);
    if (result.code !== 0) throw new Error("task engine unavailable");
    const decision = JSON.parse(result.stdout);
    if (!["allow", "deny", "continue", "observe"].includes(decision?.decision)) throw new Error("invalid task response");
    return decision;
  } catch {
    const blocked = event === "stop" || event === "pre-tool-use" && mayMutate(String(payload.toolName ?? ""), payload.input);
    return { decision: blocked ? "deny" : "observe", reason: TASK_UNAVAILABLE };
  }
};

// The bootstrap's task-intent line, worded as the Claude prompt hook words it
// (`prompt_intent::render_line`): a classifier claim, never a repository fact.
// A Pixel without `classify`, a cold or remote engine (`--if-warm` exits 1), a
// slow answer, unparsable output or p < 0.5 all leave the prompt without it.
async function classifyIntent(root: string, prompt: string): Promise<string | null> {
  try {
    const operation = resolveOperation("classify");
    const { code, stdout } = await collect(root,
      [operation, "--task-intent", "--if-warm", "--json", "--metrics", "off", "--", prompt], INTENT_TIMEOUT_MS);
    return code === 0 ? intentLine(stdout) : null;
  } catch {
    return null;
  }
}

function intentLine(stdout: string): string | null {
  try {
    const verdict = JSON.parse(stdout);
    const label = verdict?.predicted;
    const p = verdict?.probs?.[label];
    const model = verdict?.snapshot?.model;
    const ops = verdict?.next_ops;
    if (typeof label !== "string" || typeof p !== "number" || !(p >= MIN_INTENT_P)) return null;
    // `next_ops` is only usable when every entry names an operation: a null,
    // an empty or a whitespace-only entry would leave the line without one.
    if (typeof model !== "string" || !Array.isArray(ops) || ops.length === 0
        || !ops.every((op) => typeof op === "string" && op.trim().length > 0)) return null;
    return `Intent (classifier verdict, not fact): ${label} p=${p.toFixed(2)} (${model}) → start with: ${ops.join(", ")}`;
  } catch {
    return null;
  }
}

function resolveOperation(name: string) {
  const available = capabilities();
  const operation = available.commands.has(name) ? name : LEGACY_OP[name];
  if (!operation || !available.commands.has(operation)) {
    throw new Error(`Pixel ${available.version} lacks operation ${name}`);
  }
  return operation;
}

function parseEvidence(output: string) {
  try { return JSON.parse(output); }
  catch { return output.split("\n").filter(Boolean).slice(0, 40).map((line) => {
    try { return JSON.parse(line); } catch { return line; }
  }); }
}

function rememberPaths(value: unknown, paths: Set<string>, root: string) {
  if (!value || typeof value !== "object") return;
  if (Array.isArray(value)) { value.forEach((item) => rememberPaths(item, paths, root)); return; }
  const object = value as Record<string, unknown>;
  if (typeof object.path === "string" && inRepo(root, object.path)) paths.add(relativeTarget(root, object.path));
  Object.values(object).forEach((item) => rememberPaths(item, paths, root));
}

function health(root: string, action: Action) {
  const status = JSON.parse(run(root, ["status", "--json"], true));
  if (!status.index?.base_files) {
    throw new Error("Pixel index unavailable; run `pixel build-index --history .`, then retry");
  }
  if (["list_areas", "find_code", "impact", "pack_context"].includes(action) && !status.graph?.present) {
    throw new Error("Pixel graph unavailable; run `pixel rebuild-graph .`, then retry");
  }
  return { version: capabilities().version, index: status.index, graph: status.graph, facts: status.facts };
}

function audit(root: string, kind: string, reason: string, extra: Record<string, unknown> = {}) {
  try {
    mkdirSync(resolve(root, ".pixel"), { recursive: true });
    appendFileSync(resolve(root, ".pixel/pi-policy.jsonl"), JSON.stringify({
      time: new Date().toISOString(), kind, reason, ...extra,
    }) + "\n");
  } catch { /* Logging must never loosen or break the policy. */ }
}

function inRepo(root: string, path: string) {
  const rel = relative(root, isAbsolute(path) ? path : resolve(root, path));
  return rel === "" || (rel !== ".." && !rel.startsWith("../") && !isAbsolute(rel));
}

function relativeTarget(root: string, path: string) {
  return relative(root, isAbsolute(path) ? path : resolve(root, path));
}

function latestUserText(ctx: any) {
  const entries = ctx.sessionManager.getBranch();
  const last = [...entries].reverse().find((entry: any) => entry.type === "message" && entry.message?.role === "user");
  const content = last?.message?.content;
  if (typeof content === "string") return content;
  return Array.isArray(content)
    ? content.filter((part: any) => part.type === "text").map((part: any) => part.text).join(" ")
    : "";
}

function renderExecutionRoute(stdout: string): string | null {
  try {
    const route = JSON.parse(stdout)?.route;
    if (!route || typeof route.first_command !== "string" || !route.first_command.trim()
      || !Array.isArray(route.steps) || route.steps.length < 3) return null;
    const steps = route.steps.map((step: any) => {
      const order = Number(step.order);
      const action = typeof step.action === "string" ? step.action : "step";
      const command = typeof step.command === "string" ? step.command : "";
      const limit = Number(step.max_lines);
      return `${order}. ${action}${Number.isFinite(limit) && limit > 0 ? ` (maximum ${limit} lines)` : ""}: ${command}`;
    });
    const retry = route.retry?.command
      ? `\nRetry only if the first result is unresolved, capped, or irrelevant: ${route.retry.command}` : "";
    const fallback = route.fallback?.command
      ? `\nAfter two nonconverging Pixel calls, use: ${route.fallback.command}` : "";
    const unavailable = typeof route.on_unavailable === "string"
      ? `\nIf Pixel is unavailable: ${route.on_unavailable}` : "";
    return `DETERMINISTIC PIXEL ROUTE (index is a lower bound):\n${steps.join("\n")}${retry}${fallback}${unavailable}`;
  } catch {
    return null;
  }
}

function authorized(action: Action, ctx: any) {
  if (action !== "commit" && action !== "commit_and_push") return true;
  const text = latestUserText(ctx).toLowerCase().trim();
  if (/\b(?:do not|don't|never|without|no)\s+(?:\w+\s+){0,3}(?:commit|push|ship)\b/.test(text)) return false;
  const instruction = /(?:^|[.!?;]\s*)(?:please\s+)?(commit(?:\s+(?:and|&)\s+push)?|ship it|push (?:the|this|my) commit)\b/;
  const politeRequest = /\b(?:please|can you|could you|would you|i authorize you to|i approve you to)\s+(commit(?:\s+(?:and|&)\s+push)?|ship it|push (?:the|this|my) commit)\b/;
  if (/\bcommit messages?\b/.test(text)) return false;
  const request = text.match(instruction)?.[1] ?? text.match(politeRequest)?.[1] ?? "";
  if (action === "commit") return /^(commit|ship it)/.test(request);
  return /^(commit\s+(?:and|&)\s+push|ship it|push)/.test(request);
}

function commandFor(action: Action, p: any): string[][] {
  const target = String(p.query ?? p.goal ?? "").trim();
  const path = p.path ? [String(p.path)] : [];
  const subject = action === "impact" || action === "pack_context" ? (p.symbol ?? target) : target;
  if (["scope_task", "search_content", "find_code", "impact", "pack_context"].includes(action)
      && !String(subject).trim()) {
    throw new Error(`${action} requires a goal, query, or symbol`);
  }
  switch (action) {
    case "scope_task": return [["scope-task", String(p.goal ?? target), ...path, "--json"]];
    case "list_areas": return [["list-areas", ...path, "--json"]];
    case "search_content": return [["search-content", target, ...path, "--json", "--limit", "40"]];
    case "find_code": return [["find-code", target, ...path, "--json", "--limit", "5"]];
    case "impact": return [["impact", String(p.symbol ?? target), ...path, "--json"]];
    case "pack_context": return [["pack-context", String(p.symbol ?? target), ...path, "--json", "--budget", "1200"]];
    case "what_changed": return [["what-changed", ...path, "--json"]];
    case "review_changes": return [["review-changes", ...path, "--json"]];
    case "fetch": return [["fetch", String(p.remote ?? "origin"), ...path, "--json"]];
    case "commit": case "commit_and_push": {
      if (!p.message || !Array.isArray(p.files) || p.files.length === 0 || !p.request_id) {
        throw new Error("Commit requires message, explicit files, and request_id");
      }
      const files = p.files.flatMap((file: string) => ["--files", file]);
      const base = ["--message", String(p.message), ...files, "--request-id", String(p.request_id), "--json"];
      return [action === "commit"
        ? ["commit", ...base, ...path]
        : ["commit-and-push", ...base, String(p.remote ?? "origin"), String(p.refspec ?? "HEAD"), ...path]];
    }
  }
}

/// Split only unquoted `|`, `;`, `&`, `&&`, `||` and newlines. Mirrors
/// `split_segments` in `crates/pixel/src/guard.rs`; an unterminated quote or
/// an empty segment leaves the compound native so the host's permissions
/// stay authoritative.
function splitShellSegments(text: string): { text: string; piped: boolean }[] | undefined {
  const segments: { text: string; piped: boolean }[] = [];
  let quote: string | null = null;
  let start = 0;
  let piped = false;
  let i = 0;
  while (i < text.length) {
    const c = text[i];
    if (quote !== null) {
      if (c === quote) quote = null;
      i++;
      continue;
    }
    if (c === "'" || c === '"') {
      quote = c;
      i++;
      continue;
    }
    if (c === "|" || c === ";" || c === "&" || c === "\n") {
      const segment = text.slice(start, i).trim();
      if (!segment) return undefined;
      segments.push({ text: segment, piped });
      const doubled = (c === "|" || c === "&") && text[i + 1] === c;
      const end = doubled ? i + 1 : i;
      piped = c === "|" && !doubled;
      start = end + 1;
      i = end + 1;
      continue;
    }
    i++;
  }
  if (quote !== null) return undefined;
  const last = text.slice(start).trim();
  if (!last) return undefined;
  segments.push({ text: last, piped });
  return segments;
}

/// The bounded shell grammar of `search_compat::shell_argv`: quotes are
/// accepted, expansions/escapes/redirection/unquoted-globs are not.
function tokenizeShell(segment: string, literalSingleQuotes = false): string[] | undefined {
  const args: string[] = [];
  let current = "";
  let started = false;
  let quote: string | null = null;
  for (let i = 0; i < segment.length; i++) {
    const c = segment[i];
    if (c === "\0") return undefined;
    if (literalSingleQuotes && quote === "'") {
      if (c === "'") quote = null;
      else current += c;
      continue;
    }
    if (c === "\n" || c === "\r" || c === "\\" || c === "$" || c === "`") return undefined;
    if (quote !== null) {
      if (c === quote) quote = null;
      else current += c;
      continue;
    }
    if (c === "'" || c === '"') {
      quote = c;
      started = true;
      continue;
    }
    if (";|<>&()*?[]{}~#".includes(c)) return undefined;
    if (c === " " || c === "\t") {
      if (started) {
        args.push(current);
        current = "";
        started = false;
      }
      continue;
    }
    if (/\s/.test(c)) return undefined;
    current += c;
    started = true;
  }
  if (quote !== null) return undefined;
  if (started) args.push(current);
  return args;
}

/// The canonical-path containment check that backs every reader leaf: a
/// path is a repo read only when its canonical form sits inside the
/// canonical repository root. Missing files, home expansion, `..` escapes,
/// FIFOs and symlinks out of the root all stay native.
function argReadsRepo(root: string, path: string): boolean {
  if (!path || path === "-") return false;
  const absolute = isAbsolute(path) ? path : resolve(root, path);
  let canonical: string;
  try { canonical = realpathSync(absolute); }
  catch { return false; }
  let canonicalRoot: string;
  try { canonicalRoot = realpathSync(root); }
  catch { return false; }
  return canonical === canonicalRoot || canonical.startsWith(canonicalRoot + sep);
}

/// `awk` may write via redirect, pipe or `system()`: refuse a denial when
/// the script itself could be doing more than reading.
function awkMayWrite(args: string[]): boolean {
  return args.some((arg) => arg.includes(">") || arg.includes("|") || arg.includes("system("));
}

/// `sed -i`/`-ni`/`-i.bak`/`--in-place`: a write, never a plain read.
function sedEditsInPlace(args: string[]): boolean {
  return args.some((arg) => {
    if (arg.startsWith("--in-place")) return true;
    if (arg.startsWith("-") && !arg.startsWith("--") && arg.includes("i")) return true;
    return false;
  });
}

/// The canonical path of a readable repository file: a regular file inside
/// the root, never under `.git`/`.pixel` and not credential-shaped where it
/// really lives, so an in-repo symlink to `.env` is refused. Port of
/// `readable_repo_file` (`crates/pixel/src/guard.rs`).
function readableRepoFile(root: string, path: string): boolean {
  if (!path || path === "-") return false;
  let canonical: string;
  let canonicalRoot: string;
  try {
    canonicalRoot = realpathSync(root);
    canonical = realpathSync(isAbsolute(path) ? path : resolve(root, path));
  } catch { return false; }
  const rel = relative(canonicalRoot, canonical);
  if (!rel || rel === ".." || rel.startsWith("../") || isAbsolute(rel)) return false;
  const head = rel.split(sep)[0];
  if (head === ".git" || head === ".pixel") return false;
  if (CREDENTIAL_PATH.test(rel)) return false;
  try { return statSync(canonical).isFile(); }
  catch { return false; }
}

/// `sed -n 'A,Bp' F` with a bounded line window: the follow-up to a Pixel
/// hit. Never denied; the path must name a readable repository file, the
/// whole-segment test `is_bounded_sed_read` applies in the Rust guard.
function isBoundedSedRead(root: string, args: string[]): boolean {
  // Expect: ['-n', 'A,Bp', 'file']
  if (args[0] !== "-n") return false;
  if (args.length !== 3) return false;
  const range = args[1];
  if (!range || !range.endsWith("p")) return false;
  const inside = range.slice(0, -1);
  const parts = inside.split(",");
  if (parts.length !== 2) return false;
  const start = Number(parts[0]);
  const end = Number(parts[1]);
  if (!Number.isInteger(start) || !Number.isInteger(end)) return false;
  if (start < 1 || end < start || end - start > 199) return false;
  const path = args[2];
  if (!path || path.startsWith("-")) return false;
  if (CREDENTIAL_PATH.test(path)) return false;
  return readableRepoFile(root, path);
}

const REPO_READ_REASON = "repository read: use pixel search-content or pixel pack-context <uid>";
const REPO_SEARCH_REASON = "repository search: use pixel search-content";
const LS_REASON = "repository discovery: use pixel list-areas or find-code";
const FIND_REASON = "repository discovery: use pixel find-code or list-areas";
const CREDENTIAL_REASON = "credential path";

/// Port of `enforce_leaf` (`crates/pixel/src/guard.rs`). One Bash leaf at a
/// time: the first matching rule returns `{reason, operation?}`. Returning
/// `undefined` keeps the command native.
function enforceLeaf(segment: string, words: string[], piped: boolean, root: string): { reason: string; operation?: string } | undefined {
  const [bin, ...args] = words;
  let effectiveBin = bin;
  let effectiveArgs = args;
  let rtkWrapped = false;
  if (bin === "rtk" && args.length > 0 && (["cat", "head", "tail", "awk", "sed", "read"] as string[]).includes(args[0])) {
    rtkWrapped = true;
    effectiveBin = args[0];
    effectiveArgs = args.slice(1);
  }
  const nonFlagPaths = (skip: number) => effectiveArgs.filter((arg) => !arg.startsWith("-")).slice(skip);
  const credentialPath = (path: string) => CREDENTIAL_PATH.test(path);
  switch (effectiveBin) {
    case "rg":
    case "grep": {
      const nonFlag = effectiveArgs.filter((arg) => !arg.startsWith("-"));
      const explicitPath = nonFlag.length === 2;
      if (piped && !explicitPath) return undefined;
      // A value-taking flag (`rg -m 1 needle src`) makes the operand count
      // three, so the path is the last non-flag word, not the second — the
      // same fallback as `enforce_leaf`. The first operand is the pattern:
      // with nothing after it (`rg needle`, `grep -rn needle`) the search
      // runs over the cwd, which is the repo, and the pattern is no path.
      const path = nonFlag.length >= 2 ? nonFlag.at(-1)! : ".";
      if (credentialPath(path)) return { reason: CREDENTIAL_REASON };
      if (!argReadsRepo(root, path)) return undefined;
      return { reason: REPO_SEARCH_REASON, operation: "search-content" };
    }
    case "git": {
      let i = 0;
      let sub: string | undefined;
      while (i < effectiveArgs.length) {
        const word = effectiveArgs[i];
        if (["-C", "-c", "--git-dir", "--work-tree", "--namespace"].includes(word)) {
          i += 2;
          continue;
        }
        if (word.startsWith("-")) {
          i++;
          continue;
        }
        sub = word;
        i++;
        break;
      }
      if (i !== effectiveArgs.length || !sub) return undefined;
      // Spelled out so every reason the audit log carries appears verbatim
      // in the source, as the other reason constants do.
      switch (sub) {
        case "status": return { reason: "repository inspection: use pixel repo-state", operation: "repo-state" };
        case "diff": return { reason: "repository inspection: use pixel review-changes", operation: "review-changes" };
        case "log": return { reason: "repository inspection: use pixel commit-history", operation: "commit-history" };
        default: return undefined;
      }
    }
    case "cat":
    case "head":
    case "tail": {
      const paths = nonFlagPaths(0);
      for (const p of paths) if (credentialPath(p)) return { reason: CREDENTIAL_REASON };
      if (!paths.some((p) => argReadsRepo(root, p))) return undefined;
      return { reason: REPO_READ_REASON, operation: "search-content" };
    }
    case "awk": {
      if (awkMayWrite(effectiveArgs)) return undefined;
      const paths = nonFlagPaths(1);
      for (const p of paths) if (credentialPath(p)) return { reason: CREDENTIAL_REASON };
      if (!paths.some((p) => argReadsRepo(root, p))) return undefined;
      return { reason: REPO_READ_REASON, operation: "search-content" };
    }
    case "sed": {
      if (sedEditsInPlace(effectiveArgs)) return undefined;
      if (isBoundedSedRead(root, effectiveArgs)) return undefined;
      const paths = nonFlagPaths(1);
      for (const p of paths) if (credentialPath(p)) return { reason: CREDENTIAL_REASON };
      if (!paths.some((p) => argReadsRepo(root, p))) return undefined;
      return { reason: REPO_READ_REASON, operation: "search-content" };
    }
    case "read": {
      if (!rtkWrapped) return undefined;
      const paths = nonFlagPaths(0);
      for (const p of paths) if (credentialPath(p)) return { reason: CREDENTIAL_REASON };
      if (!paths.some((p) => argReadsRepo(root, p))) return undefined;
      return { reason: REPO_READ_REASON, operation: "search-content" };
    }
    case "cp": {
      const operands = effectiveArgs.filter((arg) => !arg.startsWith("-"));
      if (operands.length < 2) return undefined;
      const sources = operands.slice(0, -1);
      for (const p of sources) if (credentialPath(p)) return { reason: CREDENTIAL_REASON };
      if (!sources.some((p) => argReadsRepo(root, p))) return undefined;
      return { reason: REPO_READ_REASON };
    }
    case "ls":
    case "tree": {
      const allowedFlags = new Set(["-a", "-l", "-la", "-al", "--all", "--long"]);
      for (const arg of effectiveArgs) {
        if (arg.startsWith("-") && !allowedFlags.has(arg)) return undefined;
      }
      const path = [...effectiveArgs].reverse().find((arg) => !arg.startsWith("-")) ?? ".";
      if (credentialPath(path)) return { reason: CREDENTIAL_REASON };
      if (!argReadsRepo(root, path)) return undefined;
      return { reason: LS_REASON, operation: "list-areas" };
    }
    case "find": {
      const path = effectiveArgs[0];
      if (!path) return undefined;
      if (credentialPath(path)) return { reason: CREDENTIAL_REASON };
      if (!argReadsRepo(root, path)) return undefined;
      return { reason: FIND_REASON, operation: "find-code" };
    }
    default:
      return undefined;
  }
}

/// Decide whether the bash tool's command should be blocked under
/// `enforce`. Mirrors `enforce_shell_for_provider` from the Rust guard:
/// unknown segments, `cd`/`command`/`builtin` compounds and shells outside
/// the bounded parser stay native so the host's permissions stay
/// authoritative. Returns `undefined` to allow.
function enforceLeafDecision(command: string, root: string): { reason: string; operation?: string } | undefined {
  const trimmed = command.trim();
  if (!trimmed) return undefined;
  const segments = splitShellSegments(trimmed);
  if (!segments) return undefined;
  const tokens: (string[] | undefined)[] = segments.map((segment) => tokenizeShell(segment.text));
  if (tokens.some((t) => t === undefined)) return undefined;
  // A preceding directory change alters relative operands; the Rust guard
  // leaves the compound native rather than guessing the runtime cwd.
  if (tokens.some((words) => words!.length > 0 && (["cd", "command", "builtin"] as string[]).includes(words![0]))) {
    return undefined;
  }
  for (let i = 0; i < segments.length; i++) {
    const decision = enforceLeaf(segments[i].text, tokens[i]!, segments[i].piped, root);
    if (decision) return decision;
  }
  return undefined;
}

/// `find-code` matches carry the pieces of a uid; pack-context and impact
/// accept only that form, so a bare symbol name resolves through find-code
/// first instead of erroring inside the operation.
function matchUid(match: any): string | undefined {
  if (!match?.path || !match?.symbol_kind) return undefined;
  const name = match.raw ?? match.name;
  if (!name) return undefined;
  return `${match.path}#${match.owner ? `${match.owner}::` : ""}${name}#${match.symbol_kind}`;
}

function resolveSymbolUid(root: string, wanted: string, path?: string): { uid?: string; candidates: string[] } {
  try {
    const output = run(root, ["find-code", wanted, ...(path ? [path] : []), "--json", "--limit", "5"]);
    const found = parseEvidence(output) as any;
    const candidates = (Array.isArray(found?.matches) ? found.matches : [])
      .map(matchUid)
      .filter((uid): uid is string => Boolean(uid));
    return { uid: found?.confidence === "resolved" ? candidates[0] : undefined, candidates };
  } catch {
    return { candidates: [] };
  }
}

function classify(tool: string, input: any, root: string, resolvedPaths: Set<string>, state: { pixelHealthy?: boolean; pixelCalled: boolean }): { kind: string; reason: string; operation?: string } {
  if (tool === "pixel") return { kind: "tool", reason: "structured Pixel" };
  if (EDIT_TOOLS.has(tool)) {
    // Edits stay allowed when Pixel is unhealthy: the pixel tool already
    // reports the repair path, and gating would deadlock the session.
    if (state.pixelHealthy === true && !state.pixelCalled) {
      return { kind: "blocked", reason: "Call pixel before editing: scope_task with your goal, or pack_context on the target symbol" };
    }
    return { kind: "exception", reason: "edit tool" };
  }
  if (["todo", "web_search", "web_contents", "web_answer", "bg_wait"].includes(tool)) {
    return { kind: "exception", reason: "non-repository tool" };
  }
  if (tool === "read" || tool === "view_file") {
    // Pi 0.87.1 strips a leading `@` while resolving a read path, so the
    // lexical repository and credential checks must see what it will open.
    const requested = String(input.path ?? input.file_path ?? "");
    const path = requested.startsWith("@") ? requested.slice(1) : requested;
    if (path && !inRepo(root, path)) return { kind: "exception", reason: "outside repository" };
    const target = path ? relativeTarget(root, path) : "";
    // Bootstrap-injected paths are not evidence of a Pixel call: a bounded
    // in-repo read unlocks only after the model itself called pixel or
    // pixel_project. Both tools' results reach tool_result, so the call, not
    // a harvested path, is the signal; their paths are still recorded.
    // Credential files stay blocked.
    const limit = Number(input.limit);
    const problem = !(limit > 0) ? "no limit given"
      : limit > READ_LIMIT ? `limit ${limit} exceeds ${READ_LIMIT}`
      : CREDENTIAL_PATH.test(target) ? "credential path"
      : !state.pixelCalled ? "path not resolved by pixel yet"
      : "";
    if (path && !problem) return { kind: "exception", reason: "bounded read after a Pixel call" };
    return { kind: "blocked", reason: `Read blocked: ${problem || "no path given"}. Call pixel first, then read with a limit of at most ${READ_LIMIT} lines` };
  }
  if (tool === "bash" || tool === "run_command") {
    const command = String(input.command ?? input.cmd ?? "").trim();
    // Port of the Rust guard's leaf table: block-only. Compositions,
    // unsupported shell syntax, and unknown binaries retain their host
    // semantics so the native tool stays authoritative. `powershell` is
    // out of scope (it is absent from `classify` on the Rust side).
    const decision = enforceLeafDecision(command, root);
    if (decision?.operation === "search-content" && state.pixelCalled) {
      return { kind: "exception", reason: "native search fallback after Pixel retrieval" };
    }
    if (decision) return { kind: "blocked", reason: decision.reason, operation: decision.operation };
    return { kind: "exception", reason: "native command or unsupported shell syntax" };
  }
  if (["grep", "find", "ls", "glob", "list_dir", "grep_search", "file_search"].includes(tool)) {
    const path = input.path ?? input.directory ?? input.target_directory;
    if (typeof path === "string" && !inRepo(root, path)) return { kind: "exception", reason: "outside repository" };
    if (state.pixelCalled && ["grep", "grep_search"].includes(tool)) {
      return { kind: "exception", reason: "native search fallback after Pixel retrieval" };
    }
    return { kind: "blocked", reason: "Use the pixel tool for repository retrieval" };
  }
  return { kind: "exception", reason: "unknown tool capability" };
}

function pixelText(root: string, args: string[]): string | null {
  try {
    const out = run(root, args, true);
    const trimmed = out.trim();
    if (!trimmed) return null;
    return trimmed;
  } catch {
    return null;
  }
}

function formatWhatChanged(stdout: string): string | null {
  const j = parseEvidence(stdout) as any;
  if (!j || typeof j !== "object") return null;
  const lines: string[] = [];
  lines.push(`changed_files: ${j.changed_files ?? 0}  risk: ${j.risk ?? "?"}`);
  const syms: any[] = j.symbols ?? [];
  for (const s of syms.slice(0, 15)) {
    const procs = s.processes_total ? `  (${s.processes_total} flows)` : "";
    lines.push(`  ${s.change}  ${s.name}  ${s.path}${procs}`);
  }
  if (syms.length > 15) lines.push(`  …+${syms.length - 15} more`);
  const tests: any[] = j.suggested_tests ?? [];
  if (tests.length) lines.push(`suggested tests: ${tests.slice(0, 10).join(", ")}`);
  if (j.envelope_note) lines.push(`note: ${j.envelope_note}`);
  return lines.length > 1 ? lines.join("\n") : null;
}

export default function activate(pi: ExtensionAPI) {
  const owner = Symbol("pixel-task-adapter");
  let ownerKey: string | undefined;
  let branch: string | undefined;
  let binding: TaskBinding | undefined;
  let branchUnbound = false;
  const owns = (ctx: ExtensionContext) => {
    const session = ctx.sessionManager?.getSessionId?.();
    if (!session) return true; // Missing identity is rejected by the task engine.
    const key = `${ctx.cwd}:${session}`;
    if (!taskOwners.has(key)) taskOwners.set(key, owner);
    ownerKey = key;
    return taskOwners.get(key) === owner;
  };
  const restoreBinding = (ctx: ExtensionContext) => {
    const saved = ctx.sessionManager.getBranch().filter((entry: any) => entry.type === "custom" && entry.customType === "pixel-task-binding").at(-1) as any;
    binding = typeof saved?.data?.task_id === "string" && typeof saved?.data?.attempt_id === "string" ? saved.data : undefined;
  };
  const observe = async (ctx: ExtensionContext, event: string, payload: Record<string, unknown> = {}): Promise<TaskDecision> => {
    if (!owns(ctx)) return { decision: "observe" };
    const decision = await taskEvent(ctx, event, payload, branch, binding, branchUnbound);
    if (decision.task_id && decision.attempt_id) {
      if (binding?.task_id !== decision.task_id || binding?.attempt_id !== decision.attempt_id || binding?.branch_id !== branch) {
        binding = { task_id: decision.task_id, attempt_id: decision.attempt_id, session_id: binding?.task_id === decision.task_id ? binding.session_id ?? ctx.sessionManager.getSessionId() : ctx.sessionManager.getSessionId(), branch_id: branch };
        pi.appendEntry("pixel-task-binding", binding);
      }
      branchUnbound = false;
    }
    return decision;
  };
  pi.on("session_start", async (_event, ctx) => {
    const marker = ctx.sessionManager.getBranch().filter((entry: any) => entry.type === "custom" && entry.customType === "pixel-task-branch").at(-1) as any;
    branch = marker?.data?.branch_id ?? ctx.sessionManager.getSessionId?.();
    restoreBinding(ctx);
    branchUnbound = Boolean(marker) && !binding;
    await observe(ctx, "session-start");
  });
  pi.on("session_tree", async (event, ctx) => {
    restoreBinding(ctx);
    branch = event.newLeafId ?? ctx.sessionManager.getSessionId();
    branchUnbound = !binding;
    if (owns(ctx)) pi.appendEntry("pixel-task-branch", { branch_id: branch });
    await observe(ctx, "session-start");
  });
  pi.on("session_shutdown", async (_event, ctx) => {
    await observe(ctx, "session-end");
    if (ownerKey && taskOwners.get(ownerKey) === owner) taskOwners.delete(ownerKey);
  });
  pi.on("message_end", async (event, ctx) => {
    if (event.message.role !== "assistant") return;
    const requests = event.message.content.filter((part) => part.type === "toolCall");
    const leaf = ctx.sessionManager.getLeafId();
    await observe(ctx, "model-response", {
      event_id: leaf ? `${ctx.sessionManager.getSessionId()}:${leaf}:model-response` : undefined,
      request_ids: requests.map((part: any) => part.id),
      request_tools: Object.fromEntries(requests.map((part: any) => [part.id, part.name])),
      response_id: event.message.responseId,
      usage: event.message.usage ? {
        input: event.message.usage.input,
        output: event.message.usage.output,
        cache_read: event.message.usage.cacheRead,
        cache_write: event.message.usage.cacheWrite,
      } : undefined,
      coverage_complete: requests.every((part: any) => typeof part.id === "string" && part.id.length > 0),
      stop_reason: event.message.stopReason,
    });
  });
  pi.on("user_bash", async (_event, ctx) => {
    // Both ! and !! stay private: record only that an external action occurred.
    await observe(ctx, "user-bash");
  });
  pi.on("agent_before_settle", async (event, ctx) => {
    if (!owns(ctx)) return;
    if (ctx.signal?.aborted || event.outcome !== "completed") {
      await observe(ctx, "interrupt", { cancelled: true });
      return;
    }
    const decision = await observe(ctx, "stop", { stop_reason: event.outcome });
    if (ctx.signal?.aborted || decision.decision === "allow" || decision.decision === "observe") return;
    return {
      entries: [{ type: "custom_message" as const, customType: "pixel-task-gate", content: decision.reason ?? TASK_UNAVAILABLE, display: true }],
      continue: decision.decision === "continue" && event.context.canContinue,
    };
  });
  const resolvedPaths = new Set<string>();
  const state: { pixelHealthy?: boolean; pixelCalled: boolean } = { pixelCalled: false };
  pi.on("session_start", async () => {
    resolvedPaths.clear();
    state.pixelHealthy = undefined;
    state.pixelCalled = false;
    installed = undefined;
  });
  // Keep a distinct name from the global Pixel extension: Pi 0.87.1 rejects
  // duplicate tool names during startup, before session_start can inspect them.
  pi.registerTool({
    name: "pixel_project", label: "Pixel (project)",
    description: "Repository retrieval and Git interface. Provide the desired outcome as goal; actions are stable across Pixel CLI versions.",
    parameters: Type.Object({
      action: Type.Union(ACTIONS.map((a) => Type.Literal(a))),
      goal: Type.Optional(Type.String()),
      query: Type.Optional(Type.String()),
      path: Type.Optional(Type.String()),
      symbol: Type.Optional(Type.String()),
      remote: Type.Optional(Type.String()),
      refspec: Type.Optional(Type.String()),
      files: Type.Optional(Type.Array(Type.String())),
      message: Type.Optional(Type.String()),
      request_id: Type.Optional(Type.String()),
    }),
    async execute(_id, p, _signal, _update, ctx) {
      const root = ctx.cwd;
      const action = p.action as Action;
      try {
        if (!authorized(action, ctx)) {
          const result = { action, error: `User authorization for ${action} is absent from the current request`, next_action: "Ask the user for explicit authorization" };
          audit(root, "blocked", action, { reason: "authorization absent" });
          return { content: [{ type: "text", text: JSON.stringify(result) }], details: result, isError: true };
        }
        const index = health(root, action);
        let ambiguousCandidates: string[] = [];
        if ((action === "impact" || action === "pack_context")) {
          const wanted = String(p.symbol ?? p.query ?? p.goal ?? "").trim();
          if (wanted && !wanted.includes("#")) {
            const resolved = resolveSymbolUid(root, wanted, p.path ? String(p.path) : undefined);
            if (resolved.uid) p = { ...p, symbol: resolved.uid };
            else if (action === "pack_context") ambiguousCandidates = resolved.candidates;
          }
        }
        const steps = commandFor(action, p);
        const boxes: string[] = [];
        const evidence = steps.map((args) => {
          const { stdout: output, box } = runBox(root, args);
          if (box) boxes.push(box);
          rememberPaths(parseEvidence(output), resolvedPaths, root);
          return { operation: args[0], output: parseEvidence(output.slice(0, MAX_OUTPUT)), truncated: output.length > MAX_OUTPUT };
        });
        if (action === "find_code" && /\b(investigate|analy[sz]e|understand)\b/i.test(String(p.goal ?? ""))) {
          const found = evidence[0].output as any;
          const matches = found?.matches;
          if (found?.confidence === "resolved" && Array.isArray(matches) && matches.length === 1 && matches[0].symbol_kind) {
            const match = matches[0];
            const uid = matchUid(match)!;
            for (const args of [["impact", uid, "--json"], ["pack-context", uid, "--json", "--budget", "1200"]]) {
              const { stdout: output, box } = runBox(root, args);
              if (box) boxes.push(box);
              rememberPaths(parseEvidence(output), resolvedPaths, root);
              evidence.push({ operation: args[0], output: parseEvidence(output.slice(0, MAX_OUTPUT)), truncated: output.length > MAX_OUTPUT });
            }
          }
        }
        const truncated = evidence.some((item) => item.truncated);
        const first = evidence[0].output as any;
        const next_action = truncated ? "Narrow the scope or query"
          : ambiguousCandidates.length ? `Ambiguous symbol; call find_code to pick the target, then retry pack_context with a path#name#kind uid: ${ambiguousCandidates.join(", ")}`
          : action === "find_code" && first?.confidence !== "resolved" ? "Try search_content with a concrete token"
          : action === "search_content" && Array.isArray(first) && first.length === 0 ? "Broaden the query or check index coverage"
          : undefined;
        const result = { action, evidence, index, truncated, next_action };
        state.pixelHealthy = true;
        state.pixelCalled = true;
        audit(root, "tool", action, { index_health: "present", graph_present: index.graph?.present, facts_fresh: index.facts?.fresh, truncated });
        const content = [{ type: "text", text: JSON.stringify(result) }, ...(boxes.length ? [{ type: "text", text: boxes.join("\n") }] : [])];
        return { content, details: result };
      } catch (error) {
        state.pixelHealthy = false;
        installed = undefined;
        const result = { action, error: String(error), next_action: "Repair Pixel availability or narrow the request; native tools remain available" };
        audit(root, "unavailable", action, { reason: "Pixel unavailable or operation failed" });
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: result, isError: true };
      }
    },
  });

  // Keep both the global and project-specific Pixel tools available even when
  // a restored tool selection predates this project extension.
  const activatePixelTool = () => {
    const active = pi.getActiveTools();
    const available = new Set(pi.getAllTools().map((tool) => tool.name));
    const pixelTools = ["pixel", "pixel_project"].filter((name) => available.has(name));
    const missing = pixelTools.filter((name) => !active.includes(name));
    if (missing.length) pi.setActiveTools([...active, ...missing]);
  };
  pi.on("session_start", activatePixelTool);
  pi.on("model_select", activatePixelTool);

  // Task context is independent of optional enforcement. Parse complete
  // evidence before capping the text sent to the model.
  pi.on("before_agent_start", async (event, ctx) => {
    const root = ctx?.cwd ?? process.cwd();
    const prompt = String((event as any).prompt ?? "");
    await observe(ctx, "prompt-submit", { prompt });
    if (prompt.trim().length < MIN_PROMPT_LEN) return;
    try {
      const index = health(root, "scope_task");
      // Started first and awaited last: the classifier runs beside the two
      // context calls and never fails the bootstrap. Both are quiet probes
      // (`runAsync` passes `--metrics off`), like main's other bootstrap reads.
      const intent = classifyIntent(root, prompt);
      const routeAvailable = capabilities().commands.has("execution-brief");
      const contextOperation = routeAvailable ? "execution-brief" : "scope-task";
      const [scope, repo] = (await Promise.all([
        runAsync(root, [contextOperation, prompt, "--json", "--no-manifest", "--max-tier", "P1", "--limit", "15"]),
        runAsync(root, ["repo-state", "--json"]),
      ])).map((text) => text.trim());
      const intentText = await intent;
      for (const text of [scope, repo]) rememberPaths(parseEvidence(text), resolvedPaths, root);
      const routeText = routeAvailable ? renderExecutionRoute(scope) : null;
      state.pixelHealthy = true;
      audit(root, "bootstrap", `${contextOperation} and repo-state injected`, { graph_present: index.graph?.present, route_available: Boolean(routeText) });
      const guidance = policyFor(root) === "enforce"
        ? "Enforcement is enabled for supported native retrieval. Call the pixel tool before editing; compositions and unsupported syntax retain native behavior."
        : "Use these targets as suggestions. Native tools remain available; `pixel config policy enforce` opts into retrieval enforcement, `pixel config policy off` disables policy checks.";
      return {
        message: {
          customType: "pixel-bootstrap", display: false,
          content: `PIXEL TASK CONTEXT (deterministic, from pixel ${contextOperation} + repo-state):\n\n${routeText ?? "Pixel execution route unavailable; continue normally and keep retrieval non-blocking."}\n\n${scope.slice(0, BOOTSTRAP_BUDGET)}${scope.length > BOOTSTRAP_BUDGET ? "\n…(truncated)" : ""}\n\n${repo.slice(0, 800)}${repo.length > 800 ? "\n…(truncated)" : ""}\n\n${guidance}${intentText ? `\n\n${intentText}` : ""}`,
        },
      };
    } catch (error) {
      state.pixelHealthy = false;
      installed = undefined;
      audit(root, "bootstrap", "pixel unavailable", { reason: String(error) });
      return {
        message: {
          customType: "pixel-bootstrap", display: false,
          content: `PIXEL UNAVAILABLE: ${String(error)}. Repair Pixel when possible. Native tools remain available.`,
        },
      };
    }
  });

  pi.on("tool_call", async (event, ctx) => {
    const gate = await observe(ctx, "pre-tool-use", { toolName: event.toolName, toolCallId: event.toolCallId, input: event.input });
    if (gate.decision === "deny") return { block: true, reason: gate.reason ?? TASK_UNAVAILABLE };
    const mode = policyFor(ctx.cwd);
    if (mode === "off") return;
    const decision = classify(event.toolName, event.input, ctx.cwd, resolvedPaths, state);
    if (decision.kind === "blocked" && mode === "enforce" && state.pixelHealthy === true) {
      try { if (decision.operation) resolveOperation(decision.operation); }
      catch {
        state.pixelHealthy = false;
        installed = undefined;
        audit(ctx.cwd, "advisory", "Pixel capability unavailable; native tool preserved", { tool: event.toolName });
        return;
      }
      audit(ctx.cwd, "blocked", decision.reason, { tool: event.toolName });
      return { block: true, reason: JSON.stringify({ policy: "pixel", redirect: decision.reason }) };
    }
    audit(ctx.cwd, decision.kind === "blocked" ? "advisory" : decision.kind, decision.reason, { tool: event.toolName });
    return undefined;
  });

  // A tool_result replacement must carry forward the original result. Only
  // successful edits have a post-edit snapshot; failures retain diagnostics.
  pi.on("tool_result", async (event, ctx) => {
    const root = ctx?.cwd ?? process.cwd();
    await observe(ctx, event.isError ? "tool-failure" : "post-tool-use", {
      toolName: event.toolName, toolCallId: event.toolCallId, input: event.input, isError: event.isError,
    });
    // The global `pixel` tool never runs pixel_project.execute; its result is
    // the only signal that the model consulted Pixel. Pi reports a tool's
    // returned `isError` as false unless it throws, so an error-shaped
    // payload is recognized here instead of trusting the flag.
    if (event.toolName === "pixel" || event.toolName === "pixel_project") {
      const details = event.details as { error?: unknown } | undefined;
      if (event.isError || Boolean(details?.error)) return { isError: true };
      state.pixelCalled = true;
      // Paths the global `pixel` tool (or this one) surfaced count as
      // resolved: the guard should see every Pixel answer, not only this
      // extension's own calls.
      for (const part of event.content ?? []) {
        if (part?.type === "text") rememberPaths(parseEvidence(String(part.text)), resolvedPaths, root);
      }
    }
    if (!EDIT_TOOLS.has(event.toolName) || event.isError) return;
    const raw = pixelText(root, ["what-changed", "--json", "--tests"]);
    if (!raw) {
      state.pixelHealthy = false;
      installed = undefined;
      return;
    }
    const compact = formatWhatChanged(raw);
    if (!compact) return;
    return {
      content: [...event.content, { type: "text", text: `PIXEL BLAST RADIUS (pixel what-changed):\n\n${compact}\n\nEdits are unverified until the project's build/tests run.` }],
      details: event.details,
      isError: event.isError,
    };
  });
}
