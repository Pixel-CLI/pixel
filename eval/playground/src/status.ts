// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Is the fixture's graph and index ready, stale or missing? The source is
// `<bin> status --json` (one cheap daemon call); a binary without it falls back
// to the two freshness checks of `doctor --json`. Results are cached for 10 s.
// A refresh is `<bin> prepare-repo .`, one at a time, in the background.
import { existsSync, statSync } from "node:fs";
import { join } from "node:path";

export type Freshness = "ready" | "stale" | "missing" | "unknown";
export type Status = {
  graph: Freshness;
  index: Freshness;
  detail: string;
  graph_age_secs: number | null;
  index_age_secs: number | null;
  source: "status" | "doctor" | "none";
  checked_at: string;
};
export type RefreshState = {
  state: "idle" | "running" | "done" | "error";
  bin: string | null;
  started_at: string | null;
  finished_at: string | null;
  error: string | null;
};

export type Runner = (cmd: string[], cwd: string, timeoutMs: number) => Promise<{ stdout: string; stderr: string; code: number | null; timedOut: boolean }>;

const asRec = (v: unknown): Record<string, unknown> => (v && typeof v === "object" ? (v as Record<string, unknown>) : {});

/** `status --json` -> freshness. Pure. */
export const parseStatusJson = (rec: Record<string, unknown>): Pick<Status, "graph" | "index" | "detail"> | null => {
  if (!("graph" in rec) && !("index" in rec)) return null;
  const graph = asRec(rec.graph);
  const index = asRec(rec.index);
  const head = asRec(rec.snapshot).head;
  const oid = index.commit_oid;
  const indexBuilt = typeof index.base_files === "number" && index.base_files > 0;
  const indexStale = indexBuilt && typeof head === "string" && typeof oid === "string" && head !== oid;
  const failures = Number(asRec(rec.watcher).graph_update_failures ?? 0);
  const notes: string[] = [];
  if (indexStale) notes.push(`index is at ${String(oid).slice(0, 8)}, HEAD is ${String(head).slice(0, 8)}`);
  if (failures > 0) notes.push(`${failures} graph update failure(s) since the daemon started`);
  const gp = graph.present === true;
  const g: Freshness = !gp ? "missing" : indexStale || failures > 0 ? "stale" : "ready";
  const i: Freshness = !indexBuilt ? "missing" : indexStale ? "stale" : "ready";
  const bits = [
    gp ? `graph ${graph.files} files, ${graph.symbols} symbols` : "graph not built",
    indexBuilt ? `index ${index.base_files} files at ${String(oid).slice(0, 8)}` : "index not built",
    ...(Number(index.delta_files ?? 0) + Number(index.overlay_files ?? 0) > 0 ? [`${index.delta_files} delta / ${index.overlay_files} overlay files`] : []),
    ...notes,
  ];
  return { graph: g, index: i, detail: bits.join("; ") };
};

/** `doctor --json` freshness checks -> freshness (older binaries). Pure. */
export const parseDoctorJson = (rec: Record<string, unknown>): Pick<Status, "graph" | "index" | "detail"> | null => {
  const checks = Array.isArray(rec.checks) ? (rec.checks as Record<string, unknown>[]) : null;
  if (!checks) return null;
  const of = (id: string): Freshness => {
    const c = checks.find((x) => x.id === id);
    if (!c) return "unknown";
    if (c.status === "green") return "ready";
    return /not built|absent|missing/i.test(String(c.summary)) ? "missing" : "stale";
  };
  const line = (id: string) => String(checks.find((x) => x.id === id)?.summary ?? `${id} not checked`);
  return { graph: of("graph.freshness"), index: of("index.freshness"), detail: `${line("graph.freshness")}; ${line("index.freshness")}` };
};

const mtimeAge = (path: string): number | null => {
  try {
    return existsSync(path) ? Math.max(0, Math.round((Date.now() - statSync(path).mtimeMs) / 1000)) : null;
  } catch {
    return null;
  }
};

const parseJson = (s: string): Record<string, unknown> | null => {
  for (const c of [s.trim(), ...s.split("\n").filter((l) => l.startsWith("{"))]) {
    try {
      const v = JSON.parse(c);
      if (v && typeof v === "object" && !Array.isArray(v)) return v as Record<string, unknown>;
    } catch { /* next */ }
  }
  return null;
};

export const createStatus = (repo: string, exeOf: (bin: string) => string, run: Runner, timeoutMs = 5000) => {
  const cache = new Map<string, { at: number; value: Status }>();
  let refresh: RefreshState = { state: "idle", bin: null, started_at: null, finished_at: null, error: null };

  const compute = async (bin: string): Promise<Status> => {
    const exe = exeOf(bin);
    let parsed: ReturnType<typeof parseStatusJson> = null;
    let source: Status["source"] = "none";
    const s = await run([exe, "status", ".", "--json", "--metrics", "off"], repo, timeoutMs);
    if (s.code === 0) {
      const rec = parseJson(s.stdout);
      parsed = rec ? parseStatusJson(rec) : null;
      if (parsed) source = "status";
    }
    if (!parsed) {
      const d = await run([exe, "doctor", ".", "--json", "--only", "index.freshness", "--only", "graph.freshness"], repo, timeoutMs);
      const rec = d.code !== null ? parseJson(d.stdout) : null;
      parsed = rec ? parseDoctorJson(rec) : null;
      if (parsed) source = "doctor";
    }
    return {
      graph: parsed?.graph ?? "unknown",
      index: parsed?.index ?? "unknown",
      detail: parsed?.detail ?? "this binary reports neither `status --json` nor `doctor --json`",
      graph_age_secs: mtimeAge(join(repo, ".pixel", "graph.v2.db")),
      index_age_secs: mtimeAge(join(repo, ".pixel", "base.shard")),
      source,
      checked_at: new Date().toISOString(),
    };
  };

  const get = async (bin: string, maxAgeMs = 10_000): Promise<Status> => {
    const hit = cache.get(bin);
    if (hit && Date.now() - hit.at < maxAgeMs) return hit.value;
    const value = await compute(bin);
    cache.set(bin, { at: Date.now(), value });
    return value;
  };

  const startRefresh = (bin: string, timeout = 600_000): RefreshState & { already: boolean } => {
    if (refresh.state === "running") return { ...refresh, already: true };
    refresh = { state: "running", bin, started_at: new Date().toISOString(), finished_at: null, error: null };
    void run([exeOf(bin), "prepare-repo", "."], repo, timeout).then((r) => {
      cache.clear();
      refresh = {
        ...refresh,
        state: r.code === 0 ? "done" : "error",
        finished_at: new Date().toISOString(),
        error: r.code === 0 ? null : r.timedOut ? "prepare-repo timed out" : (r.stderr.trim().slice(-300) || `exit ${r.code}`),
      };
    });
    return { ...refresh, already: false };
  };

  return { get, startRefresh, refreshState: () => refresh };
};
