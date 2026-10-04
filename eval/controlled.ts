// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

/** Isolated backend for the existing eval/ scorer and candidate gate. */
import { createHash, randomUUID } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile, cp, chmod, lstat, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

export type Host = "claude" | "codex" | "pi";
export type Arm = "retrieval" | "gates" | "gates_classifier";
export type FileInput = { path: string; contents: string; executable?: boolean };
export type Runner = {
  host: Host; version: string; version_argv: string[]; argv: string[];
  model: string; model_config: Record<string, unknown>; permissions: Record<string, unknown>;
  environment: Record<string, string>; files: FileInput[];
};
export type Gateway = {
  kind: "model_gateway"; endpoint: string; request_path: string; model: string;
  credential_env: string; auth_header: string; auth_prefix: string;
};
export type Suite = {
  schema_version: 1; id: string; image: string; gateway_image: string; output_dir: string;
  order_seed?: string;
  repetitions: number; timeout_ms: number; max_interactions: number;
  runners: Runner[]; arms: Arm[]; network: { kind: "offline" } | Gateway;
  cases: { id: string; prompt: string; source_files: FileInput[]; contract: unknown;
    rubric: { must: { pattern: string; points: number }[]; never?: { pattern: string; penalty?: number }[] };
    verifier: { argv: string[]; files: FileInput[]; timeout_ms: number } }[];
};
type NativeResult = { answer: string; answered: boolean; turns: number | null;
  input_tokens: number | null; gen_tokens: number | null; cost_usd: number | null;
  observed_requests: number; coverage: "complete" | "partial"; missing: string[] };
type Json = Record<string, any>;
export const hash = (value: string) => createHash("sha256").update(value).digest("hex");
/** Seeded permutations, rotated so every arm occupies each position per 3 reps. */
export const armOrder = (seed: string, repetition: number, matchedCase: string): Arm[] => {
  const order: Arm[] = ["retrieval", "gates", "gates_classifier"];
  const bytes = createHash("sha256").update(JSON.stringify([seed, matchedCase, Math.floor(repetition / 3)])).digest();
  for (let i = order.length - 1; i > 0; i -= 1) { const j = bytes[i] % (i + 1); [order[i], order[j]] = [order[j], order[i]]; }
  const offset = repetition % 3; return [...order.slice(offset), ...order.slice(0, offset)];
};
const assert: (ok: unknown, message: string) => asserts ok = (ok, message) => { if (!ok) throw new Error(message); };
const identifier = (value: string) => /^[a-zA-Z0-9_-]{1,80}$/.test(value);
const relative = (value: string) => value.length > 0 && !value.startsWith("/") && !value.includes("\\") && !value.split("/").some(p => p === ".." || p === "." || !p) && !value.includes("\0");
const argvValid = (args: string[]) => Array.isArray(args) && args.length > 0 && args.every(a => typeof a === "string" && !a.includes("\0")) && args[0].length > 0;
const pinned = (image: string) => /^[a-zA-Z0-9][a-zA-Z0-9._:/-]*@sha256:[a-f0-9]{64}$/.test(image);
const forbiddenEnv = /^(ANTHROPIC_API_KEY|HOME|PATH|LD_.*|DYLD_.*|DOCKER_.*|PIXEL_.*|BUN_.*|NODE_OPTIONS)$/;
export const validateSuite = (suite: Suite) => {
  assert(suite.schema_version === 1 && identifier(suite.id), "unsupported suite schema/id");
  assert(suite.order_seed === undefined || (typeof suite.order_seed === "string" && suite.order_seed.length > 0 && suite.order_seed.length <= 256), "order seed must be a nonempty bounded string");
  assert(pinned(suite.image) && pinned(suite.gateway_image), "evaluation images must be pinned by digest");
  assert(Number.isInteger(suite.repetitions) && suite.repetitions > 0 && suite.repetitions <= 100, "repetitions must be 1..100");
  assert(Number.isInteger(suite.timeout_ms) && suite.timeout_ms > 0 && suite.timeout_ms <= 86_400_000, "required timeout must be 1..86400000ms");
  assert(Number.isInteger(suite.max_interactions) && suite.max_interactions > 0 && suite.max_interactions <= 100_000, "required interaction budget must be 1..100000");
  assert(JSON.stringify([...suite.arms].sort()) === JSON.stringify(["gates", "gates_classifier", "retrieval"]), "suite must compare retrieval, gates, gates_classifier exactly once");
  assert(suite.cases.length > 0 && suite.cases.length <= 100 && new Set(suite.cases.map(c => c.id)).size === suite.cases.length, "unique nonempty scenarios required");
  assert(suite.runners.length > 0 && suite.runners.length <= 3 && new Set(suite.runners.map(r => r.host)).size === suite.runners.length, "unique host runners required");
  for (const runner of suite.runners) {
    assert(["claude", "codex", "pi"].includes(runner.host) && runner.version.length > 0 && runner.model.length > 0, "host/version/model must be explicit");
    assert(argvValid(runner.argv) && argvValid(runner.version_argv), "runner argv/version argv required");
    assert(runner.argv.some(a => a.includes("{prompt}")), "runner argv must include {prompt}");
    if (suite.network.kind === "model_gateway") assert(runner.model === suite.network.model, "runner model must match the approved gateway model");
    for (const [key, value] of Object.entries(runner.environment)) {
      const placeholder = ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN", "OPENAI_API_KEY"].includes(key) && value === "gateway-placeholder";
      assert(/^[A-Z][A-Z0-9_]*$/.test(key) && !forbiddenEnv.test(key) && (!/TOKEN|SECRET|PASSWORD|API_KEY/.test(key) || placeholder), "runner environment cannot carry credentials or override isolation");
      assert(typeof value === "string" && !value.includes("\0"), "invalid runner environment value");
    }
  }
  for (const c of suite.cases) {
    assert(identifier(c.id) && c.prompt.length > 0 && argvValid(c.verifier.argv), "invalid scenario or heldout verifier");
    assert(Number.isInteger(c.verifier.timeout_ms) && c.verifier.timeout_ms > 0 && c.verifier.timeout_ms <= 86_400_000, "heldout timeout required");
    assert(c.rubric.must.length > 0, "existing eval rubric must contain at least one criterion");
  }
  for (const files of [...suite.runners.map(r => r.files), ...suite.cases.flatMap(c => [c.source_files, c.verifier.files])]) {
    assert(new Set(files.map(f => f.path)).size === files.length && files.every(f => relative(f.path) && typeof f.contents === "string"), "file manifests must contain unique safe relative paths");
  }
  if (suite.network.kind === "model_gateway") validateGateway(suite.network);
  else assert(suite.network.kind === "offline", "unsupported evaluation network");
};

export const validateGateway = (config: Gateway) => {
  const target = new URL(config.endpoint);
  assert(target.protocol === "https:" && !target.username && !target.password && !target.hash && !target.search, "gateway target must be one fixed HTTPS inference endpoint");
  assert(target.hostname.includes(".") && !/^(localhost|127\.|10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.|169\.254\.|\[)/.test(target.hostname) && !target.hostname.endsWith(".localhost"), "gateway target cannot be a local address");
  assert(/^\/[a-zA-Z0-9_./-]+$/.test(config.request_path) && !config.request_path.includes(".."), "gateway path must be exact");
  assert(/^[A-Z][A-Z0-9_]*$/.test(config.credential_env) && config.credential_env !== "ANTHROPIC_API_KEY", "use a narrow registered OAuth/provider credential");
  assert(/^[a-zA-Z0-9-]+$/.test(config.auth_header) && !/[\r\n]/.test(config.auth_prefix) && config.model.length > 0, "invalid gateway authentication/model");
};

/** A fixed inference path is the only egress from the internal worker network. */
export const gatewayFetch = (config: Gateway, credential: string, forward: typeof fetch = fetch) => async (request: Request) => {
  const url = new URL(request.url);
  if (request.method !== "POST" || url.pathname !== config.request_path || url.search) return new Response("denied", { status: 403 });
  const raw = await request.text();
  if (raw.length > 16 * 1024 * 1024) return new Response("request too large", { status: 413 });
  let body: Json;
  try { body = JSON.parse(raw); } catch { return new Response("invalid JSON", { status: 400 }); }
  if (!body || typeof body !== "object" || body.model !== config.model) return new Response("model denied", { status: 403 });
  const headers = new Headers({ "content-type": "application/json", [config.auth_header]: config.auth_prefix + credential });
  // Only protocol version/beta negotiation survives; caller credentials/URLs do not.
  for (const name of ["anthropic-version", "anthropic-beta", "openai-beta"]) {
    const value = request.headers.get(name); if (value) headers.set(name, value);
  }
  const response = await forward(config.endpoint, { method: "POST", body: raw, headers, redirect: "manual", signal: request.signal });
  if (response.status >= 300 && response.status < 400) return new Response("redirect denied", { status: 502 });
  return new Response(response.body, { status: response.status, headers: { "content-type": response.headers.get("content-type") ?? "application/json" } });
};

/** Native streams are secondary evidence; Codex execution items omit blocked requests. */
export const parseNative = (host: Host, text: string): NativeResult => {
  const result: NativeResult = { answer: "", answered: false, turns: null, input_tokens: null, gen_tokens: null, cost_usd: null, observed_requests: 0, coverage: "partial", missing: [] };
  const calls = new Set<string>(), messages = new Set<string>();
  let failed = false, completed = false, turns = 0, input = 0, output = 0, usageComplete = true;
  for (const [index, line] of text.split("\n").entries()) {
    if (!line.trim()) continue;
    let ev: Json; try { ev = JSON.parse(line); } catch { result.missing.push(`invalid JSON line ${index + 1}`); continue; }
    if (!ev || typeof ev !== "object" || Array.isArray(ev)) { result.missing.push(`invalid event line ${index + 1}`); continue; }
    if (host === "claude") {
      if (ev.type === "assistant") {
        const message = ev.message ?? {};
        const key = message.id ?? ev.uuid ?? String(index);
        if (!messages.has(key)) { messages.add(key); turns += 1; }
        for (const block of Array.isArray(message.content) ? message.content : []) if (block?.type === "tool_use") calls.add(`${ev.parent_tool_use_id ?? "root"}:${block.id ?? index}`);
      }
      if (ev.type === "result") {
        completed = true; failed ||= ev.subtype !== "success" || ev.is_error === true;
        result.answer = ev.result ?? ""; result.turns = ev.num_turns ?? null;
        result.input_tokens = ev.usage?.input_tokens ?? null; result.gen_tokens = ev.usage?.output_tokens ?? null;
        result.cost_usd = ev.total_cost_usd ?? null;
      }
    } else if (host === "pi") {
      if (ev.type === "message_end" && ev.message?.role === "assistant") {
        const message = ev.message, key = message.id ?? `${message.timestamp ?? index}:${JSON.stringify(message.content)}`;
        if (messages.has(key)) continue;
        messages.add(key); turns += 1;
        failed ||= ["error", "aborted"].includes(message.stopReason);
        for (const block of Array.isArray(message.content) ? message.content : []) {
          if (!block) continue;
          if (block.type === "toolCall") calls.add(block.id ?? `${key}:${calls.size}`);
          if (block.type === "text") result.answer += block.text ?? "";
        }
        usageComplete &&= Number.isFinite(message.usage?.input) && Number.isFinite(message.usage?.output);
        input += message.usage?.input ?? 0; output += message.usage?.output ?? 0;
      }
      if (ev.type === "agent_end") completed = true;
    } else {
      if (ev.type === "item.started" || ev.type === "item.completed") {
        const item = ev.item ?? {};
        if (["command_execution", "mcp_tool_call", "web_search", "file_change", "collab_tool_call"].includes(item.type)) calls.add(item.id ?? String(index));
        if (ev.type === "item.completed" && item.type === "agent_message") result.answer += item.text ?? "";
      }
      if (ev.type === "turn.completed") {
        completed = true; turns += 1;
        usageComplete &&= Number.isFinite(ev.usage?.input_tokens) && Number.isFinite(ev.usage?.output_tokens);
        input += ev.usage?.input_tokens ?? 0; output += ev.usage?.output_tokens ?? 0;
      }
      if (["turn.failed", "error"].includes(ev.type)) failed = true;
    }
  }
  if (host !== "claude") { result.turns = turns || null; result.input_tokens = usageComplete && turns ? input : null; result.gen_tokens = usageComplete && turns ? output : null; }
  result.observed_requests = calls.size;
  result.answered = completed && !failed && result.answer.trim().length > 0 && result.missing.length === 0;
  result.missing.push("native stream alone does not prove all blocked requests and child spans; durable adapter coverage required");
  return result;
};

export const telemetryMetrics = (text: string) => {
  const requests = new Map<string, string>(), spans = new Set<string>(), covered = new Set<string>(), expected = new Set<string>(), missing = new Set<string>(), finished = new Set<string>(), internals = new Set<string>();
  const observed = new Map<string, Json>();
  let blocked = 0, retried = 0, coordinator = 0, classifier = 0;
  const events = new Map<string, string>();
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    let ev: Json; try { ev = JSON.parse(line); } catch { missing.add("invalid telemetry JSON"); continue; }
    if (!ev || typeof ev !== "object" || Array.isArray(ev)) { missing.add("invalid telemetry envelope"); continue; }
    if (ev.schema_version !== 1 || !ev.event_id || !ev.task_id || !ev.attempt_id || !ev.span_id) { missing.add("invalid telemetry envelope"); continue; }
    if (!["tool_requested", "tool_finished", "model_response", "internal_call", "coverage"].includes(ev.kind)
      || (["tool_requested", "tool_finished"].includes(ev.kind) && !ev.request_id)
      || (ev.kind === "model_response" && !ev.response_id) || (ev.kind === "internal_call" && (!ev.call_id || !["coordinator", "classifier"].includes(ev.actor)))
      || (ev.kind === "coverage" && (!Array.isArray(ev.child_spans) || !Array.isArray(ev.missing)))) { missing.add("invalid telemetry observation"); continue; }
    const previous = events.get(ev.event_id);
    if (previous) { if (previous !== line) missing.add("conflicting telemetry event"); continue; }
    events.set(ev.event_id, line);
    const span = `${ev.task_id}/${ev.attempt_id}/${ev.span_id}`; spans.add(span);
    const observationKey = `${span}/${ev.kind}/${ev.request_id ?? ev.response_id ?? ev.call_id ?? ev.event_id}`;
    if (ev.kind !== "coverage") observed.set(observationKey, ev);
    if (ev.kind === "tool_requested") {
      const key = `${span}/${ev.request_id}`;
      const value = JSON.stringify([ev.tool, ev.retry_of ?? null]);
      const previous = requests.get(key);
      if (previous) {
        const [priorTool, priorRetry] = JSON.parse(previous);
        if (priorRetry !== (ev.retry_of ?? null) || (priorTool !== ev.tool && priorTool !== "unknown" && ev.tool !== "unknown")) missing.add("conflicting request identity");
      }
      if (!requests.has(key) && ev.retry_of) retried += 1;
      if (!previous || ev.tool !== "unknown") requests.set(key, value);
    }
    if (ev.kind === "tool_finished" && !finished.has(`${span}/${ev.request_id}`)) { finished.add(`${span}/${ev.request_id}`); if (ev.outcome === "blocked") blocked += 1; }
    if (ev.kind === "internal_call" && !internals.has(`${span}/${ev.call_id}`)) { internals.add(`${span}/${ev.call_id}`); if (ev.actor === "coordinator") coordinator += 1; if (ev.actor === "classifier") classifier += 1; }
    if (ev.kind === "coverage") {
      if (ev.complete === true && !(ev.missing ?? []).length) covered.add(span);
      else for (const reason of ev.missing ?? ["partial span"]) missing.add(reason);
      for (const child of ev.child_spans ?? []) expected.add(`${ev.task_id}/${ev.attempt_id}/${child}`);
    }
  }
  for (const span of new Set([...spans, ...expected])) if (!covered.has(span)) missing.add(`missing span coverage: ${span}`);
  if (events.size === 0) missing.add("telemetry unavailable");
  for (const key of finished) if (!requests.has(key)) missing.add("result without request");
  const complete = events.size > 0 && missing.size === 0;
  const duration = (kind: string, actor?: string) => {
    const values = [...observed.values()].filter(e => e.kind === kind && (!actor || e.actor === actor));
    return values.length > 0 && values.every(e => Number.isFinite(e.duration_ms) && e.duration_ms >= 0) ? values.reduce((sum, e) => sum + e.duration_ms, 0) : null;
  };
  return { coverage: complete ? "complete" : "partial", model_tool_requests: complete ? requests.size : null, observed_model_tool_requests: requests.size,
    blocked_requests: blocked, retried_requests: retried, coordinator_calls: coordinator, classifier_calls: classifier,
    tool_duration_ms: duration("tool_finished"), model_duration_ms: duration("model_response"), coordinator_duration_ms: duration("internal_call", "coordinator"), classifier_duration_ms: duration("internal_call", "classifier"), missing: [...missing] };
};

const safeEnvironment = () => Object.fromEntries(Object.entries(process.env).filter(([key]) => ["HOME", "PATH", "TMPDIR", "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG"].includes(key))) as Record<string, string>;
const command = async (argv: string[], options: { env?: Record<string, string>; timeout?: number } = {}) => {
  const child = Bun.spawn(argv, { env: { ...safeEnvironment(), ...options.env }, stdout: "pipe", stderr: "pipe" });
  const timer = setTimeout(() => child.kill("SIGKILL"), options.timeout ?? 30_000);
  try {
    const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
    assert(code === 0, `${argv[0]} ${argv[1] ?? ""} failed (${code}): ${stderr.slice(0, 1000)}${stdout.slice(0, 2000)}`);
    return stdout;
  } finally { clearTimeout(timer); }
};
const docker = (args: string[], options?: Parameters<typeof command>[1]) => command(["docker", ...args], options);
// Docker Desktop preserves copied ownership. The enclosing host scratch is 0700;
// explicit trial files become accessible in the capability-free container only.
const stage = async (root: string, files: FileInput[], writable = false) => {
  await chmod(root, writable ? 0o777 : 0o755);
  for (const f of files) {
    const path = join(root, f.path); await mkdir(dirname(path), { recursive: true });
    if (writable) { let parent = dirname(path); while (parent !== root) { await chmod(parent, 0o777); parent = dirname(parent); } }
    const mode = writable ? (f.executable ? 0o777 : 0o666) : (f.executable ? 0o755 : 0o644);
    await writeFile(path, f.contents, { mode }); await chmod(path, mode);
  }
};
const rejectLinks = async (root: string) => { for (const item of await readdir(root, { withFileTypes: true })) { const path = join(root, item.name), stat = await lstat(path); assert(!stat.isSymbolicLink() && (stat.isFile() || stat.isDirectory()), "trial output contains symlink or special file"); if (stat.isDirectory()) await rejectLinks(path); } };
const replace = (value: string, prompt: string, gateway: string) => value.replaceAll("{prompt}", prompt).replaceAll("{gateway}", gateway);
const hardened = ["--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--pids-limit", "256", "--memory", "4g", "--cpus", "2", "--tmpfs", "/tmp:rw,nosuid,nodev,size=256m", "--env", "HOME=/home/trial", "--workdir", "/workspace"];

/** Runs explicit trials only. All model-visible storage is disposable Docker storage. */
export const evaluate = async (suite: Suite, resources: { scorer: string; gate: string }) => {
  validateSuite(suite);
  const output = resolve(suite.output_dir);
  await mkdir(output, { recursive: true });
  // Exclusive identity creation prevents mixing attempts/snapshots in an old result directory.
  await writeFile(join(output, "controlled-suite.json"), JSON.stringify(suite, null, 2), { flag: "wx", mode: 0o600 });
  const scratch = await mkdtemp(join(tmpdir(), "pixel-evaluate-"));
  const prefix = `pixel-eval-${randomUUID()}`, internal = `${prefix}-private`;
  const containers: string[] = [], volumes: string[] = [];
  const rows: Json[] = [];
  const orderSeed = suite.order_seed ?? suite.id;
  let networkCreated = false;
  let cancelled = false;
  const cancel = () => { cancelled = true; void Promise.all(containers.map(name => docker(["kill", name]).catch(() => {}))); };
  process.on("SIGINT", cancel); process.on("SIGTERM", cancel);
  const cleanup = async () => {
    for (const name of containers.reverse()) await docker(["rm", "-f", name]).catch(() => {});
    for (const name of volumes.reverse()) await docker(["volume", "rm", name]).catch(() => {});
    if (networkCreated) await docker(["network", "rm", internal]).catch(() => {});
    await rm(scratch, { recursive: true, force: true });
  };
  try {
    const workerImage = JSON.parse(await docker(["image", "inspect", suite.image]))[0];
    const clearImageEnv = (details: Json) => (details.Config?.Env ?? []).flatMap((item: string) => {
      const key = item.split("=", 1)[0]; assert(key !== "ANTHROPIC_API_KEY", "image contains a banned provider credential setting; use a clean image"); return key === "PATH" ? [] : ["--env", `${key}=`];
    });
    const workerEnv = clearImageEnv(workerImage);
    const gatewayUrl = "http://model-gateway:8080";
    if (suite.network.kind === "model_gateway") {
      const config = suite.network, credential = process.env[config.credential_env];
      assert(credential, `missing registered provider credential: ${config.credential_env}`);
      await docker(["network", "create", "--internal", internal]); networkCreated = true;
      const gateway = `${prefix}-gateway`; containers.push(gateway);
      const gatewayImage = JSON.parse(await docker(["image", "inspect", suite.gateway_image]))[0];
      await docker(["create", "--name", gateway, "--network", internal, "--network-alias", "model-gateway", ...clearImageEnv(gatewayImage), ...hardened,
        "--env", "PIXEL_MODEL_TOKEN", "--entrypoint", "bun", suite.gateway_image, "/gateway.ts"], { env: { PIXEL_MODEL_TOKEN: credential } });
      // Only this trusted filtering service joins the outbound network; the worker never does.
      await docker(["network", "connect", "bridge", gateway]);
      const configPath = join(scratch, "gateway.json"), scriptPath = join(scratch, "gateway.ts");
      await writeFile(configPath, JSON.stringify(config));
      await writeFile(scriptPath, `import {gatewayFetch} from '/controlled.ts';\nconst c=await Bun.file('/gateway.json').json();\nBun.serve({hostname:'0.0.0.0',port:8080,idleTimeout:0,maxRequestBodySize:16777216,fetch:gatewayFetch(c,process.env.PIXEL_MODEL_TOKEN!)});\n`);
      await docker(["cp", configPath, `${gateway}:/gateway.json`]); await docker(["cp", scriptPath, `${gateway}:/gateway.ts`]);
      await docker(["cp", import.meta.path, `${gateway}:/controlled.ts`]); await docker(["start", gateway]);
      await docker(["exec", gateway, "bun", "-e", "for(let n=0;n<40;n++){try{await fetch('http://127.0.0.1:8080/');process.exit(0)}catch{await Bun.sleep(50)}}process.exit(1)"]);
    }
    const scenarios = join(output, "scenarios"); await mkdir(scenarios);
    for (const c of suite.cases) await writeFile(join(scenarios, `${c.id}.json`), JSON.stringify(c.rubric));
    for (let rep = 0; rep < suite.repetitions; rep += 1) for (const runner of suite.runners) for (const c of suite.cases) for (const arm of armOrder(orderSeed, rep, `${runner.host}/${c.id}`)) {
      assert(!cancelled, "evaluation cancelled by operator");
      const id = `${prefix}-${rows.length}`, name = `${id}-agent`, dir = join(output, `rep-${rep}`), stem = `${c.id}-${arm}.${runner.host}`;
      await mkdir(dir, { recursive: true });
      const source = join(scratch, id, "source"), config = join(scratch, id, "config"), verifier = join(scratch, id, "verifier"), final = join(scratch, id, "final");
      for (const path of [source, config, verifier, final]) await mkdir(path, { recursive: true });
      await stage(source, c.source_files, true); await stage(config, runner.files); await stage(verifier, c.verifier.files);
      await writeFile(join(config, "contract.json"), JSON.stringify(c.contract));
      for (const suffix of ["work", "home", "telemetry"]) { const volume = `${id}-${suffix}`; await docker(["volume", "create", volume]); volumes.push(volume); }
      // Set only private volume modes, before running the image's configured user.
      const setupName = `${id}-setup`; containers.push(setupName);
      await docker(["run", "--rm", "--name", setupName, "--network", "none", "--user", "0:0", ...workerEnv, ...hardened,
        "--volume", `${id}-work:/workspace`, "--volume", `${id}-home:/home/trial`, "--volume", `${id}-telemetry:/telemetry`,
        "--entrypoint", "chmod", suite.image, "0777", "/workspace", "/home/trial", "/telemetry"]);
      const env = Object.entries(runner.environment).flatMap(([key, value]) => ["--env", `${key}=${replace(value, c.prompt, gatewayUrl)}`]);
      const args = runner.argv.map(a => replace(a, c.prompt, gatewayUrl));
      containers.push(name);
      await docker(["create", "--name", name, "--network", networkCreated ? internal : "none", ...workerEnv, ...hardened,
        "--volume", `${id}-work:/workspace`, "--volume", `${id}-home:/home/trial`, "--volume", `${id}-telemetry:/telemetry`,
        ...env, "--env", `PIXEL_TASK_POLICY=${arm}`, "--env", "PIXEL_TASK_CONTRACT=/runner/contract.json", "--env", "PIXEL_TASK_TELEMETRY_PATH=/telemetry/events.jsonl",
        "--entrypoint", args[0], suite.image, ...args.slice(1)]);
      await docker(["cp", `${source}/.`, `${name}:/workspace`]); await docker(["cp", config, `${name}:/runner`]);
      // Version is checked in the same image, but in a separate process without model credentials.
      const versionName = `${id}-version`; containers.push(versionName);
      const actualVersion = (await docker(["run", "--rm", "--name", versionName, "--network", "none", ...workerEnv, ...hardened, "--entrypoint", runner.version_argv[0], suite.image, ...runner.version_argv.slice(1)])).trim();
      assert(actualVersion === runner.version, `runner version mismatch: expected ${runner.version}, got ${actualVersion}`);
      const started = Date.now(), process = Bun.spawn(["docker", "start", "--attach", name], { env: safeEnvironment(), stdout: "pipe", stderr: "pipe" });
      let stdout = "", stderr = "", terminated: string | null = null, telemetry = "", polling = false, finishedRun = false;
      const consume = async (stream: ReadableStream<Uint8Array>, output: "stdout" | "stderr") => { const decoder = new TextDecoder(); for await (const chunk of stream) { const text = decoder.decode(chunk, { stream: true }); if (output === "stdout") stdout += text; else stderr += text; if (stdout.length + stderr.length > 32 * 1024 * 1024) { terminated = "output_budget"; await docker(["kill", name]).catch(() => {}); } } };
      const monitor = setInterval(async () => {
        if (polling) return; polling = true;
        try {
          telemetry = await docker(["exec", name, "cat", "/telemetry/events.jsonl"], { timeout: 2_000 }).catch(() => telemetry);
          if (finishedRun) return;
          const count = Math.max(parseNative(runner.host, stdout).observed_requests, telemetryMetrics(telemetry).observed_model_tool_requests);
          if (Date.now() - started >= suite.timeout_ms || count > suite.max_interactions) { terminated = count > suite.max_interactions ? "interaction_budget" : "timeout"; await docker(["kill", name]).catch(() => {}); }
        } finally { polling = false; }
      }, 250);
      let code = -1;
      try { [, , code] = await Promise.all([consume(process.stdout, "stdout"), consume(process.stderr, "stderr"), process.exited]); }
      finally { finishedRun = true; clearInterval(monitor); }
      const containerState = JSON.parse(await docker(["inspect", "--format", "{{json .State}}", name]));
      if (containerState.Running || containerState.ExitCode !== 0) code = containerState.ExitCode ?? -1;
      const duration = Date.now() - started;
      if (cancelled) terminated = "cancelled";
      const telemetryPath = join(scratch, `${id}.telemetry`);
      await docker(["cp", `${name}:/telemetry/events.jsonl`, telemetryPath]).then(async () => { telemetry = await readFile(telemetryPath, "utf8"); }).catch(() => {});
      await docker(["cp", `${name}:/workspace/.`, final]); await rejectLinks(final);
      await cp(final, join(dir, `${stem}.workspace`), { recursive: true });
      const checkName = `${id}-verifier`; containers.push(checkName);
      // Heldout files enter only this fresh, offline container, after the agent exits.
      await docker(["create", "--name", checkName, "--network", "none", ...workerEnv, ...hardened, "--volume", `${id}-work:/workspace`,
        "--entrypoint", c.verifier.argv[0], suite.image, ...c.verifier.argv.slice(1)]);
      await docker(["cp", verifier, `${checkName}:/heldout`]);
      let verified = false, verification = "";
      try { verification = await docker(["start", "--attach", checkName], { timeout: c.verifier.timeout_ms });
        const state = JSON.parse(await docker(["inspect", "--format", "{{json .State}}", checkName])); verified = state.Running === false && state.ExitCode === 0;
      }
      catch (error) { verification = String(error); await docker(["kill", checkName]).catch(() => {}); }
      const native = parseNative(runner.host, stdout), trajectory = telemetryMetrics(telemetry);
      const comparison_key = hash(JSON.stringify([c.contract, c.prompt, c.source_files, runner, suite.image, suite.gateway_image, suite.network, suite.timeout_ms, suite.max_interactions, c.verifier]));
      const row = { schema_version: 1, scenario: c.id, arm, cli: runner.host, rep, comparison_key,
        order_seed: orderSeed, arm_order: armOrder(orderSeed, rep, `${runner.host}/${c.id}`),
        ...native, ...trajectory, answered: native.answered && code === 0 && !terminated && verified,
        verifier_success: verified, exit_code: code, termination: terminated ?? "exited", duration_ms: duration,
        transcript_sha256: hash(stdout), runner_version: actualVersion, image: suite.image,
        diagnostic: code === 0 && verified ? null : { stderr: stderr.slice(0, 2000), verification: verification.slice(0, 2000) } };
      await writeFile(join(dir, `${stem}.jsonl`), stdout); await writeFile(join(dir, `${stem}.err`), stderr);
      await writeFile(join(dir, `${stem}.controlled.json`), JSON.stringify(row, null, 2));
      await writeFile(join(dir, `${stem}.telemetry`), telemetry); await writeFile(join(dir, `${stem}.verification.txt`), verification);
      rows.push(row);
      await docker(["rm", "-f", name, checkName]);
    }
    const scorer = join(scratch, "score.py"), gate = join(scratch, "gate.py");
    await writeFile(scorer, resources.scorer); await writeFile(gate, resources.gate);
    const scoring = await command(["python3", scorer, "--results", output, "--scenarios-dir", scenarios]);
    await writeFile(join(output, "scoring.txt"), scoring);
    const gates = [];
    for (const candidate of ["gates", "gates_classifier"]) {
      let passed = true, detail = "";
      try { detail = await command(["python3", gate, "--results", output, "--scenarios-dir", scenarios, "--baseline", "retrieval", "--candidate", candidate]); }
      catch (error) { passed = false; detail = String(error); }
      gates.push({ candidate, passed, detail });
    }
    const report = { schema_version: 1, suite_id: suite.id, output_dir: output, order_seed: orderSeed, order_algorithm: "sha256-permutation-three-repetition-rotation-v1", rows, gates, all_passed: gates.every(g => g.passed) };
    await writeFile(join(output, "controlled-report.json"), JSON.stringify(report, null, 2));
    return report;
  } finally { process.off("SIGINT", cancel); process.off("SIGTERM", cancel); await cleanup(); }
};

if (import.meta.main) {
  try {
    const input = await new Response(Bun.stdin.stream()).json();
    const report = await evaluate(input.suite, input.resources);
    process.stdout.write(JSON.stringify(report));
  } catch (error) { process.stderr.write(`${error}\n`); process.exitCode = 1; }
}
