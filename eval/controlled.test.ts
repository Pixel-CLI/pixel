import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { armOrder, evaluate, gatewayFetch, hash, parseNative, telemetryMetrics, validateGateway, validateSuite, type Gateway, type Host, type Suite } from "./controlled";

const image = `fixture@sha256:${"a".repeat(64)}`;
const suite = (): Suite => ({ schema_version: 1, id: "fixture", image, gateway_image: image, output_dir: "/unused", repetitions: 1, timeout_ms: 5000, max_interactions: 5,
  runners: [{ host: "pi", version: "fixture-1", version_argv: ["fixture", "--version"], argv: ["fixture", "{prompt}"], model: "test", model_config: {}, permissions: { sandbox: "private" }, environment: {}, files: [] }],
  arms: ["retrieval", "gates", "gates_classifier"], network: { kind: "offline" },
  cases: [{ id: "case", prompt: "Fix", source_files: [{ path: "a.txt", contents: "before" }], contract: { objective: "fix" }, rubric: { must: [{ pattern: "fixed", points: 1 }] }, verifier: { argv: ["true"], files: [], timeout_ms: 1000 } }] });
const event = (event_id: string, data: object) => ({ schema_version: 1, event_id, task_id: "task", attempt_id: "attempt", span_id: "root", occurred_ms: 1, ...data });
const jsonl = (...events: object[]) => events.map(e => JSON.stringify(e)).join("\n") + "\n";
const native = (host: Host) => host === "pi" ? jsonl({ type: "message_end", message: { role: "assistant", id: "m", content: [{ type: "text", text: "fixed" }], usage: { input: 0, output: 2 }, stopReason: "stop" } }, { type: "agent_end" })
  : host === "claude" ? jsonl({ type: "result", subtype: "success", result: "fixed", num_turns: 1, usage: { input_tokens: 0, output_tokens: 2 } })
    : jsonl({ type: "item.completed", item: { id: "m", type: "agent_message", text: "fixed" } }, { type: "turn.completed", usage: { input_tokens: 0, output_tokens: 2 } });

describe("frozen suite boundaries", () => {
  test("seeded arm order is reproducible, a permutation, and balanced over three repetitions", () => {
    const orders = Array.from({ length: 6 }, (_, rep) => armOrder("frozen-seed", rep, "pi/case"));
    expect(orders).toEqual(Array.from({ length: 6 }, (_, rep) => armOrder("frozen-seed", rep, "pi/case")));
    for (const order of orders) expect([...order].sort()).toEqual(["gates", "gates_classifier", "retrieval"]);
    for (const block of [0, 3]) for (let position = 0; position < 3; position += 1) expect(new Set(orders.slice(block, block + 3).map(order => order[position])).size).toBe(3);
    expect(orders).not.toEqual(Array.from({ length: 6 }, (_, rep) => armOrder("different-seed", rep, "pi/case")));
  });
  test("rejects unpinned runtime, unbounded trials, duplicate arms and secrets before execution", () => {
    validateSuite(suite());
    for (const mutate of [(s: Suite) => { s.image = "latest"; }, (s: Suite) => { s.timeout_ms = 0; }, (s: Suite) => { s.max_interactions = 0; }, (s: Suite) => { s.arms = ["gates", "gates", "retrieval"]; }, (s: Suite) => { s.runners[0].environment = { DEPLOY_TOKEN: "redacted" }; }, (s: Suite) => { s.cases[0].source_files[0].path = "../escape"; }]) {
      const input = suite(); mutate(input); expect(() => validateSuite(input)).toThrow();
    }
  });
});

describe("three native host streams", () => {
  for (const host of ["claude", "codex", "pi"] as Host[]) test(`${host}: semantic success and measured zero tokens survive`, () => {
    const parsed = parseNative(host, native(host));
    expect(parsed.answered).toBe(true); expect(parsed.turns).toBe(1); expect(parsed.input_tokens).toBe(0);
    expect(parsed.coverage).toBe("partial");
  });
  test("Pi JSON exit zero cannot hide provider error or incomplete stream", () => {
    expect(parseNative("pi", jsonl({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "partial" }], stopReason: "error" } }, { type: "agent_end" })).answered).toBe(false);
    expect(parseNative("pi", native("pi").split("\n")[0]).answered).toBe(false);
  });
  test("Codex answer without turn completion and Claude max-turns error fail", () => {
    expect(parseNative("codex", jsonl({ type: "item.completed", item: { type: "agent_message", text: "partial" } })).answered).toBe(false);
    expect(parseNative("claude", jsonl({ type: "result", subtype: "error_max_turns", result: "partial" })).answered).toBe(false);
  });
  test("deduplicates native request IDs rather than counting execution and completion twice", () => {
    expect(parseNative("codex", jsonl({ type: "item.started", item: { type: "command_execution", id: "call" } }, { type: "item.completed", item: { type: "command_execution", id: "call" } })).observed_requests).toBe(1);
  });
});

describe("primary requests and completeness", () => {
  test("unknown model request label coalesces with native label; real disagreements fail", () => {
    const unknown = event("model", { kind: "tool_requested", request_id: "call", tool: "unknown" });
    const known = event("native", { kind: "tool_requested", request_id: "call", tool: "edit" });
    const complete = event("coverage", { kind: "coverage", complete: true, child_spans: [], missing: [] });
    expect(telemetryMetrics(jsonl(unknown, known, complete)).model_tool_requests).toBe(1);
    expect(telemetryMetrics(jsonl(known, unknown, complete)).model_tool_requests).toBe(1);
    expect(telemetryMetrics(jsonl(known, event("conflict", { kind: "tool_requested", request_id: "call", tool: "write" }), complete)).model_tool_requests).toBeNull();
    expect(telemetryMetrics(jsonl(known, event("retry", { kind: "tool_requested", request_id: "call", tool: "unknown", retry_of: "prior" }), complete)).model_tool_requests).toBeNull();
  });
  test("blocked requests and retries count; notifications/internal calls do not", () => {
    const request = event("one", { kind: "tool_requested", request_id: "request", tool: "edit" });
    const text = jsonl(request, request, event("two", { kind: "tool_finished", request_id: "request", outcome: "blocked" }), event("three", { kind: "tool_requested", request_id: "retry", tool: "edit", retry_of: "request" }), event("four", { kind: "internal_call", actor: "classifier", call_id: "prediction" }), event("end", { kind: "coverage", complete: true, child_spans: [], missing: [] }));
    expect(telemetryMetrics(text)).toEqual({ coverage: "complete", model_tool_requests: 2, observed_model_tool_requests: 2, blocked_requests: 1, retried_requests: 1, coordinator_calls: 0, classifier_calls: 1, tool_duration_ms: null, model_duration_ms: null, coordinator_duration_ms: null, classifier_duration_ms: null, missing: [] });
  });
  test("absent and missing-child telemetry is unknown, not zero", () => {
    expect(telemetryMetrics("").model_tool_requests).toBeNull();
    const partial = telemetryMetrics(jsonl(event("end", { kind: "coverage", complete: true, child_spans: ["child"], missing: [] })));
    expect(partial.model_tool_requests).toBeNull(); expect(partial.missing).toContain("missing span coverage: task/attempt/child");
  });
});

describe("fixed inference gateway", () => {
  const config: Gateway = { kind: "model_gateway", endpoint: "https://provider.example/v1/messages", request_path: "/v1/messages", model: "approved", credential_env: "REGISTERED_OAUTH_TOKEN", auth_header: "authorization", auth_prefix: "Bearer " };
  test("denies arbitrary paths/methods/models and cannot follow redirect", async () => {
    const calls: any[] = [];
    const proxy = gatewayFetch(config, "fixture-credential", (async (...args: any[]) => { calls.push(args); return new Response("redirect", { status: 302, headers: { location: "https://other.example" } }); }) as typeof fetch);
    expect((await proxy(new Request("http://gateway/admin"))).status).toBe(403);
    expect((await proxy(new Request("http://gateway/v1/messages", { method: "POST", body: JSON.stringify({ model: "other" }) }))).status).toBe(403);
    expect(calls).toHaveLength(0);
    expect((await proxy(new Request("http://gateway/v1/messages", { method: "POST", headers: { authorization: "caller", "x-extra": "secret" }, body: JSON.stringify({ model: "approved" }) }))).status).toBe(502);
    expect(calls[0][0]).toBe(config.endpoint); expect(calls[0][1].redirect).toBe("manual");
    expect(calls[0][1].headers.get("authorization")).toBe("Bearer fixture-credential"); expect(calls[0][1].headers.get("x-extra")).toBeNull();
  });
  test("streams approved response and rejects local endpoint/banned credential", async () => {
    validateGateway(config);
    expect(() => validateGateway({ ...config, endpoint: "https://127.0.0.1/v1/messages" })).toThrow();
    expect(() => validateGateway({ ...config, credential_env: "ANTHROPIC_API_KEY" })).toThrow();
    const proxy = gatewayFetch(config, "fixture", (async () => new Response("data: fixed\n\n", { headers: { "content-type": "text/event-stream" } })) as typeof fetch);
    const response = await proxy(new Request("http://gateway/v1/messages", { method: "POST", body: JSON.stringify({ model: "approved" }) }));
    expect(await response.text()).toBe("data: fixed\n\n"); expect(response.headers.get("content-type")).toBe("text/event-stream");
  });
});

test("existing gate checks all hosts, rejects unknown metrics and mismatched comparisons", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pixel-gate-test-"));
  try {
    const rows = ["claude", "pi", "codex"].flatMap(cli => ["retrieval", "gates"].map(arm => ({ cli, arm, scenario: "case", score: 1, answered: true, turns: 1, rep: 0, comparison_key: cli, coverage: "complete", model_tool_requests: 2, verifier_success: true })));
    const run = async () => { await writeFile(join(directory, "scores.json"), JSON.stringify(rows)); const child = Bun.spawn(["python3", join(import.meta.dir, "gate.py"), "--results", directory, "--baseline", "retrieval", "--candidate", "gates"], { stdout: "pipe", stderr: "pipe" }); return { code: await child.exited, out: await new Response(child.stdout).text() }; };
    expect((await run()).code).toBe(0);
    rows[5].turns = null as any; expect((await run()).out).toContain("codex/case: unknown turn coverage -> FAIL");
    rows[5].turns = 1; rows[5].comparison_key = "other"; expect((await run()).code).toBe(1);
    rows[5].comparison_key = "codex"; rows[5].model_tool_requests = 3; expect((await run()).out).toContain("interaction regression");
  } finally { await rm(directory, { recursive: true, force: true }); }
});

test("existing scorer refuses sidecar for altered transcript", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pixel-score-test-"));
  try {
    const transcript = join(directory, "case-gates.pi.jsonl"), content = native("pi");
    await writeFile(transcript, content);
    await writeFile(join(directory, "case-gates.pi.controlled.json"), JSON.stringify({ schema_version: 1, cli: "pi", answer: "fixed", answered: true, verifier_success: true, termination: "exited", exit_code: 0, transcript_sha256: hash(content) }));
    const run = async () => { const child = Bun.spawn(["python3", "-c", "from score import load_result; from pathlib import Path; import sys,json; print(json.dumps(load_result(Path(sys.argv[1]),'pi')))", transcript], { cwd: import.meta.dir, stdout: "pipe" }); expect(await child.exited).toBe(0); return JSON.parse(await new Response(child.stdout).text()); };
    expect((await run())[1].answered).toBe(true);
    await writeFile(transcript, content + "tampered"); expect((await run())[1].answered).toBe(false);
  } finally { await rm(directory, { recursive: true, force: true }); }
});

const dockerImage = process.env.PIXEL_EVAL_DOCKER_TEST_IMAGE;
test.skipIf(!dockerImage)("Docker fake trials reuse scorer/gate across three hosts and three arms", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pixel-controlled-test-"));
  try {
    const input = suite(); input.image = dockerImage!; input.output_dir = join(directory, "results");
    input.runners = (["claude", "codex", "pi"] as Host[]).map(host => ({ ...input.runners[0], host, version_argv: ["sh", "-c", "printf fixture-1"], argv: ["sh", "/runner/fake.sh", "{prompt}"], files: [{ path: "fake.sh", contents: `set -eu\ntest "$HOME" = /home/trial\ntest ! -e /var/run/docker.sock\ntest ! -e /heldout\nprintf fixed > /workspace/a.txt\ncat > /telemetry/events.jsonl <<'EOF'\n${jsonl(event("end", { kind: "coverage", complete: true, child_spans: [], missing: [] }))}EOF\ncat <<'EOF'\n${native(host)}EOF\n` }] }));
    input.cases[0].verifier = { argv: ["sh", "/heldout/check.sh"], timeout_ms: 5000, files: [{ path: "check.sh", contents: "set -eu\ntest \"$(cat /workspace/a.txt)\" = fixed\n" }] };
    const report = await evaluate(input, { scorer: await readFile(join(import.meta.dir, "score.py"), "utf8"), gate: await readFile(join(import.meta.dir, "gate.py"), "utf8") });
    expect(report.rows).toHaveLength(9); expect(report.rows.filter(r => !r.answered)).toEqual([]); expect(report.gates.filter(g => !g.passed)).toEqual([]); expect(report.all_passed).toBe(true);
    for (const row of report.rows) { expect(row.verifier_success).toBe(true); expect(row.model_tool_requests).toBe(0); expect(row.termination).toBe("exited"); }
    expect(JSON.parse(await readFile(join(input.output_dir, "scores.json"), "utf8"))).toHaveLength(9);
  } finally { await rm(directory, { recursive: true, force: true }); }
}, 120_000);

test.skipIf(!dockerImage)("Docker timeout and interaction budgets produce failed attempts with preserved evidence", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pixel-controlled-bounds-"));
  try {
    const input = suite(); input.image = dockerImage!; input.output_dir = join(directory, "results"); input.timeout_ms = 700; input.max_interactions = 1;
    input.runners[0] = { ...input.runners[0], version_argv: ["sh", "-c", "printf fixture-1"], argv: ["sh", "/runner/fake.sh", "{prompt}"], files: [{ path: "fake.sh", contents: `set -eu\nif test "$1" = exceed; then\ncat > /telemetry/events.jsonl <<'EOF'\n${jsonl(event("one", { kind: "tool_requested", request_id: "one", tool: "read" }), event("two", { kind: "tool_requested", request_id: "two", tool: "read" }))}EOF\nfi\nsleep 5\n` }] };
    input.cases = ["timeout", "exceed"].map(id => ({ ...input.cases[0], id, prompt: id, verifier: { argv: ["true"], files: [], timeout_ms: 1000 } }));
    const report = await evaluate(input, { scorer: await readFile(join(import.meta.dir, "score.py"), "utf8"), gate: await readFile(join(import.meta.dir, "gate.py"), "utf8") });
    expect(report.rows).toHaveLength(6); expect(report.all_passed).toBe(false);
    for (const row of report.rows) {
      expect(row.answered).toBe(false); expect(row.termination).toBe(row.scenario === "timeout" ? "timeout" : "interaction_budget");
      expect(row.duration_ms).toBeLessThan(4000); expect(row.model_tool_requests).toBeNull();
    }
  } finally { await rm(directory, { recursive: true, force: true }); }
}, 120_000);

const gatewayImage = process.env.PIXEL_EVAL_GATEWAY_TEST_IMAGE;
test.skipIf(!dockerImage || !gatewayImage)("Docker gateway admits only fixed inference route and worker has no external route or credential", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pixel-controlled-gateway-"));
  const previous = process.env.PIXEL_EVAL_TEST_OAUTH; process.env.PIXEL_EVAL_TEST_OAUTH = "nonsecret-fixture";
  try {
    const input = suite(); input.image = dockerImage!; input.gateway_image = gatewayImage!; input.output_dir = join(directory, "results"); input.timeout_ms = 10_000;
    input.network = { kind: "model_gateway", endpoint: "https://api.openai.com/v1/responses", request_path: "/v1/responses", model: "test", credential_env: "PIXEL_EVAL_TEST_OAUTH", auth_header: "authorization", auth_prefix: "Bearer " };
    input.runners[0] = { ...input.runners[0], version_argv: ["sh", "-c", "printf fixture-1"], argv: ["sh", "/runner/fake.sh", "{prompt}"], files: [{ path: "fake.sh", contents: `set -eu\ntest -z "\${PIXEL_MODEL_TOKEN:-}"\ntest -z "\${PIXEL_EVAL_TEST_OAUTH:-}"\nif wget -T 2 -O /tmp/forbidden http://model-gateway:8080/admin 2>/tmp/status; then exit 10; fi\ngrep 403 /tmp/status >/dev/null\nif wget -T 2 -O /tmp/forbidden --post-data='{"model":"not-approved"}' http://model-gateway:8080/v1/responses 2>/tmp/status; then exit 11; fi\ngrep 403 /tmp/status >/dev/null\nif wget -T 1 -O /tmp/internet http://1.1.1.1 2>/dev/null; then exit 12; fi\ncat > /telemetry/events.jsonl <<'EOF'\n${jsonl(event("end", { kind: "coverage", complete: true, child_spans: [], missing: [] }))}EOF\ncat <<'EOF'\n${native("pi")}EOF\n` }] };
    const report = await evaluate(input, { scorer: await readFile(join(import.meta.dir, "score.py"), "utf8"), gate: await readFile(join(import.meta.dir, "gate.py"), "utf8") });
    expect(report.rows.filter(row => !row.answered).map(row => row.diagnostic)).toEqual([]); expect(report.all_passed).toBe(true);
  } finally { if (previous === undefined) delete process.env.PIXEL_EVAL_TEST_OAUTH; else process.env.PIXEL_EVAL_TEST_OAUTH = previous; await rm(directory, { recursive: true, force: true }); }
}, 120_000);
