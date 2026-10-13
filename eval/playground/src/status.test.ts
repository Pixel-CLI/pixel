// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Self-test of /api/status and /api/refresh against a fake `pixel` that prints
// canned `status --json` / `doctor --json` output and counts its calls.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { handlers, type Config } from "./api";
import { parseDoctorJson, parseStatusJson } from "./status";

const HEAD = "a".repeat(40);
const status = (over: Record<string, unknown> = {}) => ({
  graph: { present: true, files: 5, symbols: 9 },
  index: { base_files: 10, commit_oid: HEAD, delta_files: 0, overlay_files: 0 },
  snapshot: { head: HEAD, dirty_count: 0 },
  watcher: { graph_update_failures: 0 },
  ...over,
});

describe("parseStatusJson", () => {
  test("ready", () => {
    expect(parseStatusJson(status())).toMatchObject({ graph: "ready", index: "ready" });
  });
  test("index behind HEAD makes both stale and names the commits", () => {
    const r = parseStatusJson(status({ snapshot: { head: "b".repeat(40) } }))!;
    expect(r).toMatchObject({ graph: "stale", index: "stale" });
    expect(r.detail).toContain("aaaaaaaa");
    expect(r.detail).toContain("bbbbbbbb");
  });
  test("graph update failures make the graph stale only", () => {
    expect(parseStatusJson(status({ watcher: { graph_update_failures: 2 } }))).toMatchObject({ graph: "stale", index: "ready" });
  });
  test("missing graph and missing index", () => {
    expect(parseStatusJson(status({ graph: { present: false } }))).toMatchObject({ graph: "missing", index: "ready" });
    expect(parseStatusJson(status({ index: { base_files: 0 } }))).toMatchObject({ index: "missing" });
  });
  test("not a status record", () => {
    expect(parseStatusJson({ checks: [] })).toBeNull();
  });
});

describe("parseDoctorJson", () => {
  const check = (id: string, status: string, summary: string) => ({ id, status, summary });
  test("green, red 'not built', yellow other", () => {
    const r = parseDoctorJson({ checks: [check("graph.freshness", "red", "graph not built"), check("index.freshness", "yellow", "index old")] })!;
    expect(r).toMatchObject({ graph: "missing", index: "stale" });
    expect(parseDoctorJson({ checks: [check("graph.freshness", "green", "ok")] })).toMatchObject({ graph: "ready", index: "unknown" });
    expect(parseDoctorJson({})).toBeNull();
  });
});

let dir: string;
let cfg: Config;
let api: ReturnType<typeof handlers>;
const script = (name: string, body: string) => {
  const p = join(dir, name);
  writeFileSync(p, `#!/bin/sh\n${body}\n`);
  chmodSync(p, 0o755);
  return p;
};
const count = (f: string) => (existsSync(join(dir, f)) ? readFileSync(join(dir, f), "utf8").trim().split("\n").length : 0);
const get = async (bin: string) => {
  const res = await api.status(new Request(`http://x/api/status?bin=${encodeURIComponent(bin)}`));
  return { status: res.status, body: await res.json() };
};
const refresh = async (bin: string) => (await api.refresh(new Request("http://x/api/refresh", { method: "POST", body: JSON.stringify({ bin }) }))).json();

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "playground-status-"));
  mkdirSync(join(dir, "repo/.pixel"), { recursive: true });
  writeFileSync(join(dir, "repo/.pixel/graph.v2.db"), "x");
  const stale = JSON.stringify(status({ snapshot: { head: "b".repeat(40) } }));
  const doctor = JSON.stringify({ checks: [{ id: "graph.freshness", status: "red", summary: "graph not built" }, { id: "index.freshness", status: "green", summary: "index present (3s old)" }] });
  const fakeStatus = script("fake-status", `case "$1" in status) echo x >> "${dir}/status.calls"; cat <<'J'\n${stale}\nJ\n;; prepare-repo) echo x >> "${dir}/prepare.calls"; sleep 0.6; echo built >> "${dir}/graph.built";; esac`);
  const fakeOld = script("fake-old", `case "$1" in status) echo "error: unexpected argument '--json'" >&2; exit 2;; doctor) cat <<'J'\n${doctor}\nJ\n;; prepare-repo) echo boom >&2; exit 4;; esac`);
  const fakeNone = script("fake-none", `echo nothing; exit 2`);
  cfg = {
    repo: join(dir, "repo"), promptsFile: "", localPromptsFile: "", binsFile: "", timeoutMs: 1000,
    defaultBins: [{ name: "s", path: fakeStatus }, { name: "o", path: fakeOld }, { name: "n", path: fakeNone }],
    codex: { codexBin: "/bin/false", root: dir, userCodexHome: dir, repo: join(dir, "repo"), inject: "hook", runTimeoutMs: 1000 },
  };
  api = handlers(cfg);
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("GET /api/status", () => {
  test("parses status --json, reports the graph age, caches for 10 s", async () => {
    const a = (await get(cfg.defaultBins[0]!.path)).body;
    expect(a).toMatchObject({ ok: true, graph: "stale", index: "stale", source: "status", refresh: { state: "idle" } });
    expect(a.graph_age_secs).toBeGreaterThanOrEqual(0);
    expect(a.index_age_secs).toBeNull();
    expect(Date.parse(a.checked_at)).not.toBeNaN();
    await get(cfg.defaultBins[0]!.path);
    expect(count("status.calls")).toBe(1);
  });
  test("falls back to doctor for a binary without status --json", async () => {
    expect((await get(cfg.defaultBins[1]!.path)).body).toMatchObject({ graph: "missing", index: "ready", source: "doctor" });
  });
  test("a binary with neither is unknown, not an error", async () => {
    expect((await get(cfg.defaultBins[2]!.path)).body).toMatchObject({ graph: "unknown", index: "unknown", source: "none" });
  });
  test("unknown binary is refused", async () => {
    expect((await get("/bin/echo")).status).toBe(404);
  });
});

describe("POST /api/refresh", () => {
  test("runs prepare-repo once at a time, then clears the cache", async () => {
    const bin = cfg.defaultBins[0]!.path;
    const first = await refresh(bin);
    expect(first).toMatchObject({ ok: true, state: "running", already: false });
    expect(await refresh(bin)).toMatchObject({ state: "running", already: true });
    expect((await get(bin)).body.refresh.state).toBe("running");
    for (let i = 0; i < 40 && (await get(bin)).body.refresh.state === "running"; i++) await Bun.sleep(100);
    expect((await get(bin)).body.refresh).toMatchObject({ state: "done", error: null });
    expect(count("prepare.calls")).toBe(1);
    expect(count("status.calls")).toBe(2); // the cache was cleared by the refresh
  });
  test("a failing prepare-repo reports its error", async () => {
    const bin = cfg.defaultBins[1]!.path;
    await refresh(bin);
    let st = (await get(bin)).body.refresh;
    for (let i = 0; i < 40 && st.state === "running"; i++) { await Bun.sleep(100); st = (await get(bin)).body.refresh; }
    expect(st).toMatchObject({ state: "error", error: "boom" });
  });
});
