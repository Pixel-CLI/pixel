// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Read-only endpoints over census.db: what agents do in a session (searches,
// reads, edits...) before they answer. History sessions are private: their
// prompts and final answers are never selected, and no output leaves this
// module longer than 4 KB.
import { Database } from "bun:sqlite";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

export const CENSUS_DIR = "/private/tmp/claude-501/census";
export const OUTPUT_CAP = 4096;
export const SOURCES = ["census", "claude-history", "codex-history", "pi-history"] as const;

export type NeedGroup = "search" | "definition" | "usages" | "read" | "list" | "edit" | "test" | "git" | "web" | "other";
export const GROUP_CODE: Record<NeedGroup, string> = {
  search: "s", definition: "d", usages: "u", read: "r", list: "l", edit: "e", test: "t", git: "g", web: "w", other: "o",
};
const GROUPS: Record<string, NeedGroup> = {
  "search-regex": "search", "search-string": "search", "find-definition": "definition", "find-usages": "usages",
  "read-region": "read", "read-whole-file": "read", "read-docs": "read", "read-config": "read",
  "list-files": "list", "list-tree": "list", "edit-code": "edit",
  "run-tests": "test", "build-typecheck": "test", "git-status": "git", "git-history": "git", "web-lookup": "web",
};
export const needGroup = (need: string | null): NeedGroup => GROUPS[need ?? ""] ?? "other";
const EXPLORE = new Set<NeedGroup>(["search", "definition", "usages", "read", "list"]);

/** Calls that look around before the first edit; with no edit, all of them. */
export const exploreBefore = (needs: (string | null)[]): number => {
  let n = 0;
  for (const need of needs) {
    const g = needGroup(need);
    if (g === "edit") break;
    if (EXPLORE.has(g)) n++;
  }
  return n;
};

/** Upper bounds of the histogram buckets shared by both distributions. */
export const BUCKETS = [0, 1, 3, 6, 10, 20, 40, 80, Infinity];
export const bucketLabel = (i: number) =>
  i === 0 ? "0" : i === BUCKETS.length - 1 ? `${BUCKETS[i - 1]! + 1}+` : BUCKETS[i - 1]! + 1 === BUCKETS[i] ? String(BUCKETS[i]) : `${BUCKETS[i - 1]! + 1}-${BUCKETS[i]}`;
const histogram = (values: number[]) => {
  const counts = BUCKETS.map(() => 0);
  for (const v of values) counts[BUCKETS.findIndex((b) => v <= b)]!++;
  return BUCKETS.map((_, i) => ({ bucket: bucketLabel(i), count: counts[i]! }));
};

const json = (body: unknown, status = 200) => Response.json(body, { status });
const fail = (error: string, status = 400) => json({ ok: false, error }, status);

const truncate = (text: string | null) => {
  const bytes = new TextEncoder().encode(text ?? "");
  return bytes.length <= OUTPUT_CAP ? (text ?? "") : new TextDecoder().decode(bytes.slice(0, OUTPUT_CAP));
};

type SessionRow = {
  id: string; source: string; repo: string | null; sha: string | null; category: string | null; mode: string | null;
  prompt: string | null; wall_s: number | null; cost_usd: number | null; tokens_in: number | null; tokens_out: number | null;
};

export const createCensus = (dir = CENSUS_DIR) => {
  const dbPath = join(dir, "census.db");
  let db: Database | null = null;
  const open = (): Database | null => {
    if (db) return db;
    if (!existsSync(dbPath)) return null;
    return (db = new Database(dbPath, { readonly: true }));
  };
  const cache = new Map<string, { at: number; body: unknown }>();
  const cached = <T>(key: string, make: () => T): T => {
    const hit = cache.get(key);
    if (hit && Date.now() - hit.at < 30_000) return hit.body as T;
    const body = make();
    cache.set(key, { at: Date.now(), body });
    return body;
  };
  const noDb = () => fail(`census database not found: ${dbPath}`, 503);
  const sourceOf = (u: URL) => {
    const s = u.searchParams.get("source") ?? "census";
    return (SOURCES as readonly string[]).includes(s) ? s : null;
  };

  const sessions = (req: Request) => {
    const u = new URL(req.url);
    const source = sourceOf(u);
    if (!source) return fail(`source must be one of ${SOURCES.join(", ")}`);
    const d = open();
    if (!d) return noDb();
    const category = u.searchParams.get("category") ?? "";
    const repo = u.searchParams.get("repo") ?? "";
    const body = cached(`sessions:${source}:${category}:${repo}`, () => {
      // The prompt is selected only for census sessions: history prompts are
      // private and never leave the database.
      const rows = d.query<SessionRow, [string, string, string, string, string]>(
        `SELECT id, source, repo, sha, category, mode,
                CASE WHEN source = 'census' THEN prompt END AS prompt,
                wall_s, cost_usd, tokens_in, tokens_out
         FROM sessions WHERE source = ? AND (? = '' OR category = ?) AND (? = '' OR repo = ?)
         ORDER BY (started_at IS NULL), started_at DESC, id`,
      ).all(source, category, category, repo, repo);
      // One cheap pass over the need of the first 80 calls of each session.
      const calls = d.query<{ session: string; need: string | null }, [string, string, string, string, string]>(
        `SELECT c.session, c.need FROM calls c JOIN sessions s ON s.id = c.session
         WHERE s.source = ? AND (? = '' OR s.category = ?) AND (? = '' OR s.repo = ?)
         ORDER BY c.session, c.seq`,
      ).all(source, category, category, repo, repo);
      const needs = new Map<string, (string | null)[]>();
      for (const c of calls) (needs.get(c.session) ?? needs.set(c.session, []).get(c.session)!).push(c.need);
      return {
        ok: true,
        source,
        sessions: rows.map((r) => {
          const ns = needs.get(r.id) ?? [];
          return {
            id: r.id, source: r.source, repo: r.repo, sha: r.sha, category: r.category, mode: r.mode,
            prompt: r.source === "census" ? r.prompt : null,
            private: r.source !== "census",
            wall_s: r.wall_s, cost_usd: r.cost_usd, tokens: (r.tokens_in ?? 0) + (r.tokens_out ?? 0),
            n_calls: ns.length,
            explore_before_first_edit_or_answer: exploreBefore(ns),
            strip: ns.slice(0, 80).map((n) => GROUP_CODE[needGroup(n)]).join(""),
          };
        }),
        categories: d.query<{ category: string }, [string]>("SELECT DISTINCT category FROM sessions WHERE source = ? AND category IS NOT NULL ORDER BY 1").all(source).map((r) => r.category),
        repos: d.query<{ repo: string }, [string]>("SELECT DISTINCT repo FROM sessions WHERE source = ? AND repo IS NOT NULL ORDER BY 1").all(source).map((r) => r.repo),
      };
    });
    return json(body);
  };

  const session = (req: Request) => {
    const id = decodeURIComponent(new URL(req.url).pathname.split("/").pop() ?? "");
    const d = open();
    if (!d) return noDb();
    const s = d.query<SessionRow & { final_answer: string | null }, [string]>(
      `SELECT id, source, repo, sha, category, mode,
              CASE WHEN source = 'census' THEN prompt END AS prompt,
              wall_s, cost_usd, tokens_in, tokens_out,
              CASE WHEN source = 'census' THEN final_answer END AS final_answer
       FROM sessions WHERE id = ?`,
    ).get(id);
    if (!s) return fail(`no such session: ${id}`, 404);
    const calls = d.query<{ seq: number; tool: string | null; class: string | null; need: string | null; input_json: string | null; output: string | null; output_bytes: number | null; is_error: number | null }, [string]>(
      "SELECT seq, tool, class, need, input_json, output, output_bytes, is_error FROM calls WHERE session = ? ORDER BY seq",
    ).all(id);
    const diffFile = s.source === "census" && /^[\w.-]+$/.test(id) ? join(dir, "diffs", `${id}.diff`) : null;
    return json({
      ok: true,
      session: { ...s, private: s.source !== "census", tokens: (s.tokens_in ?? 0) + (s.tokens_out ?? 0) },
      calls: calls.map((c) => ({
        seq: c.seq, tool: c.tool, class: c.class, need: c.need, group: needGroup(c.need), input_json: c.input_json,
        output: truncate(c.output), output_bytes: c.output_bytes ?? new TextEncoder().encode(c.output ?? "").length,
        is_error: !!c.is_error,
      })),
      final_answer: s.final_answer,
      diff: diffFile && existsSync(diffFile) ? readFileSync(diffFile, "utf8").slice(0, 200_000) : null,
    });
  };

  const summary = (req: Request) => {
    const source = sourceOf(new URL(req.url));
    if (!source) return fail(`source must be one of ${SOURCES.join(", ")}`);
    const d = open();
    if (!d) return noDb();
    return json(cached(`summary:${source}`, () => {
      const byNeed = d.query<{ need: string | null; n: number }, [string]>(
        "SELECT c.need, COUNT(*) n FROM calls c JOIN sessions s ON s.id = c.session WHERE s.source = ? GROUP BY c.need ORDER BY n DESC",
      ).all(source);
      const byClass = d.query<{ class: string | null; n: number }, [string]>(
        "SELECT c.class, COUNT(*) n FROM calls c JOIN sessions s ON s.id = c.session WHERE s.source = ? GROUP BY c.class ORDER BY n DESC",
      ).all(source);
      const perSession = new Map<string, (string | null)[]>();
      for (const r of d.query<{ session: string; need: string | null }, [string]>(
        "SELECT c.session, c.need FROM calls c JOIN sessions s ON s.id = c.session WHERE s.source = ? ORDER BY c.session, c.seq",
      ).all(source)) (perSession.get(r.session) ?? perSession.set(r.session, []).get(r.session)!).push(r.need);
      const sessionRows = d.query<{ id: string; category: string | null }, [string]>("SELECT id, category FROM sessions WHERE source = ?").all(source);
      const groups: Record<string, number> = {};
      for (const r of byNeed) groups[needGroup(r.need)] = (groups[needGroup(r.need)] ?? 0) + r.n;
      const cats = new Map<string, { sessions: number; calls: number; explore: number }>();
      for (const s of sessionRows) {
        const ns = perSession.get(s.id) ?? [];
        const c = cats.get(s.category ?? "(none)") ?? { sessions: 0, calls: 0, explore: 0 };
        c.sessions++; c.calls += ns.length; c.explore += exploreBefore(ns);
        cats.set(s.category ?? "(none)", c);
      }
      const counts = sessionRows.map((s) => (perSession.get(s.id) ?? []).length);
      return {
        ok: true,
        source,
        sessions: sessionRows.length,
        calls: byNeed.reduce((a, r) => a + r.n, 0),
        by_need: byNeed.map((r) => ({ need: r.need ?? "(none)", group: needGroup(r.need), n: r.n })),
        by_class: byClass.map((r) => ({ class: r.class ?? "(none)", n: r.n })),
        by_group: Object.entries(groups).map(([group, n]) => ({ group, n })).sort((a, b) => b.n - a.n),
        calls_per_session: histogram(counts),
        explore_before: histogram(sessionRows.map((s) => exploreBefore(perSession.get(s.id) ?? []))),
        by_category: [...cats].map(([category, c]) => ({ category, sessions: c.sessions, avg_calls: c.calls / c.sessions, avg_explore: c.explore / c.sessions })).sort((a, b) => b.sessions - a.sessions),
      };
    }));
  };

  return { sessions, session, summary };
};
