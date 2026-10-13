// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Self-test of the census endpoints against a tiny fixture database.
import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createCensus, exploreBefore, needGroup } from "./census";

let dir: string;
let census: ReturnType<typeof createCensus>;
const get = async (h: (r: Request) => Response, path: string) => {
  const res = h(new Request(`http://x${path}`));
  return { status: res.status, body: await res.json() };
};

beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), "playground-census-"));
  mkdirSync(join(dir, "diffs"));
  const db = new Database(join(dir, "census.db"));
  db.run(`CREATE TABLE sessions(id TEXT PRIMARY KEY, source TEXT, repo TEXT, sha TEXT, category TEXT, mode TEXT, prompt TEXT, prompt_hash TEXT, started_at TEXT, wall_s REAL, model TEXT, tokens_in INTEGER, tokens_out INTEGER, cost_usd REAL, final_answer TEXT, ok INTEGER, raw_path TEXT)`);
  db.run(`CREATE TABLE calls(session TEXT, seq INTEGER, tool TEXT, input_json TEXT, output TEXT, output_bytes INTEGER, is_error INTEGER, class TEXT, need TEXT, ts TEXT, turn INTEGER, call_id TEXT, PRIMARY KEY(session, seq))`);
  const s = db.prepare("INSERT INTO sessions(id,source,repo,category,mode,prompt,wall_s,tokens_in,tokens_out,cost_usd,final_answer) VALUES (?,?,?,?,?,?,?,?,?,?,?)");
  s.run("c-1", "census", "rg", "lookup", "edit", "fix the flag", 9.5, 100, 20, 0.04, "done: patched");
  s.run("c-2", "census", "rg", "history", "answer", "when did it change", 3, 10, 5, 0.01, "in 2019");
  s.run("h-1", "claude-history", "pixel", "lookup", "answer", "SECRET HISTORY PROMPT", 5, 1, 1, 0, "SECRET HISTORY ANSWER");
  const c = db.prepare("INSERT INTO calls(session,seq,tool,input_json,output,output_bytes,is_error,class,need) VALUES (?,?,?,?,?,?,?,?,?)");
  const big = "x".repeat(10_000);
  for (const [i, tool, cls, need, out] of [
    [0, "Grep", "search", "search-regex", "hit"], [1, "Read", "read", "read-region", big],
    [2, "Read", "read", "read-docs", "doc"], [3, "Edit", "edit", "edit-code", "ok"], [4, "Grep", "search", "find-usages", "later"],
  ] as const) c.run("c-1", i, tool, JSON.stringify({ pattern: "p" }), out, out.length, 0, cls, need);
  c.run("c-2", 0, "Bash", "{}", "log", 3, 1, "git-read", "git-history");
  c.run("h-1", 0, "Read", "{}", big, big.length, 0, "read", "read-region");
  c.run("h-1", 1, "Grep", "{}", "y", 1, 0, "search", "search-string");
  db.close();
  writeFileSync(join(dir, "diffs/c-1.diff"), "--- a\n+++ b\n");
  census = createCensus(dir);
});
afterAll(() => rmSync(dir, { recursive: true, force: true }));

describe("grouping", () => {
  test("needs map to display groups; exploring stops at the first edit", () => {
    expect(needGroup("find-usages")).toBe("usages");
    expect(needGroup("run-tests")).toBe("test");
    expect(needGroup("nonsense")).toBe("other");
    expect(needGroup(null)).toBe("other");
    expect(exploreBefore(["search-regex", "read-region", "git-status", "edit-code", "read-region"])).toBe(2);
    expect(exploreBefore(["search-regex", "other", "list-tree"])).toBe(2);
  });
});

describe("GET /api/census/sessions", () => {
  test("defaults to census: prompts, strip, counts, explore-before", async () => {
    const { body } = await get(census.sessions, "/api/census/sessions");
    expect(body.sessions.map((r: { id: string }) => r.id).sort()).toEqual(["c-1", "c-2"]);
    const c1 = body.sessions.find((r: { id: string }) => r.id === "c-1");
    expect(c1).toMatchObject({ prompt: "fix the flag", private: false, n_calls: 5, tokens: 120, explore_before_first_edit_or_answer: 3, strip: "srreu" });
    expect(body.categories).toEqual(["history", "lookup"]);
  });
  test("history sessions never carry a prompt", async () => {
    const { body } = await get(census.sessions, "/api/census/sessions?source=claude-history");
    expect(body.sessions).toHaveLength(1);
    expect(body.sessions[0]).toMatchObject({ prompt: null, private: true, n_calls: 2 });
    expect(JSON.stringify(body)).not.toContain("SECRET");
  });
  test("filters by category and repo; rejects an unknown source", async () => {
    const { body } = await get(census.sessions, "/api/census/sessions?category=history");
    expect(body.sessions.map((r: { id: string }) => r.id)).toEqual(["c-2"]);
    expect((await get(census.sessions, "/api/census/sessions?repo=nope")).body.sessions).toEqual([]);
    expect((await get(census.sessions, "/api/census/sessions?source=x")).status).toBe(400);
  });
});

describe("GET /api/census/session/:id", () => {
  test("ordered calls, output cut at 4 KB with the true size, final answer and diff", async () => {
    const { body } = await get(census.session, "/api/census/session/c-1");
    expect(body.calls.map((c: { seq: number }) => c.seq)).toEqual([0, 1, 2, 3, 4]);
    expect(body.calls[1]).toMatchObject({ need: "read-region", group: "read", output_bytes: 10_000 });
    expect(body.calls[1].output.length).toBe(4096);
    expect(body.final_answer).toBe("done: patched");
    expect(body.diff).toContain("+++ b");
    expect((await get(census.session, "/api/census/session/c-2")).body.diff).toBeNull();
  });
  test("history: no prompt, no answer, output still capped", async () => {
    const { body } = await get(census.session, "/api/census/session/h-1");
    expect(body.session).toMatchObject({ prompt: null, private: true });
    expect(body.final_answer).toBeNull();
    expect(body.diff).toBeNull();
    expect(body.calls[0].output.length).toBe(4096);
    expect(JSON.stringify(body)).not.toContain("SECRET");
  });
  test("unknown id is 404; a path-like id never reads outside diffs", async () => {
    expect((await get(census.session, "/api/census/session/zzz")).status).toBe(404);
    expect((await get(census.session, "/api/census/session/..%2Fx")).status).toBe(404);
  });
});

describe("GET /api/census/summary", () => {
  test("counts by group and class, histograms, by category", async () => {
    const { body } = await get(census.summary, "/api/census/summary");
    expect(body).toMatchObject({ source: "census", sessions: 2, calls: 6 });
    expect(Object.fromEntries(body.by_group.map((g: { group: string; n: number }) => [g.group, g.n]))).toEqual({ read: 2, search: 1, usages: 1, edit: 1, git: 1 });
    expect(body.by_class.find((c: { class: string }) => c.class === "read").n).toBe(2);
    expect(body.calls_per_session.reduce((a: number, b: { count: number }) => a + b.count, 0)).toBe(2);
    expect(body.explore_before.find((b: { bucket: string }) => b.bucket === "2-3").count).toBe(1);
    expect(body.by_category.map((c: { category: string }) => c.category).sort()).toEqual(["history", "lookup"]);
  });
  test("history summary is aggregate only", async () => {
    const { body } = await get(census.summary, "/api/census/summary?source=claude-history");
    expect(body).toMatchObject({ sessions: 1, calls: 2 });
    expect(JSON.stringify(body)).not.toContain("SECRET");
  });
  test("a missing database is a 503, not a crash", async () => {
    const none = createCensus(join(dir, "nowhere"));
    expect((await get(none.summary, "/api/census/summary")).status).toBe(503);
  });
});
