/**
 * Golden envelope shapes + LIVE round-trip through the real Rust binary:
 * every envelope this package emits must parse with
 * `pixel_sniper::types::ReportEnvelope` and come back intact from
 * `pixel sniper last/env --json`.
 */
import { beforeAll, describe, expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { ErrorEnvelope, EventEnvelope, RunEnvelope } from "../src/report.ts";

const repoRoot = resolve(import.meta.dir, "..", "..", "..");
const binPath = join(repoRoot, "target", "debug", "pixel");

beforeAll(() => {
  if (!existsSync(binPath)) {
    execFileSync("cargo", ["build", "-p", "pixel-cli"], {
      cwd: repoRoot,
      stdio: "inherit",
      timeout: 600_000,
    });
  }
});

const makeSandbox = () => {
  const stateRoot = mkdtempSync(join(tmpdir(), "sniper-state-"));
  const project = mkdtempSync(join(tmpdir(), "sniper-proj-"));
  const env = { ...process.env, GITPIXEL_SNIPER_STATE_ROOT: stateRoot };
  const run = (args: string[], input?: string) => {
    const result = spawnSync(binPath, args, {
      input,
      env,
      encoding: "utf8",
      timeout: 30_000,
    });
    return result;
  };
  return { project, run };
};

// Golden envelopes — exactly what the adapters emit.
const goldenError: ErrorEnvelope = {
  surface: "browser-rejection",
  message: "TypeError: undefined is not an object (evaluating 'api.sessions.list')",
  kind: "TypeError",
  stack_raw: "useApi@http://localhost:5173/src/api.ts:12:9",
  frames: [
    {
      raw: "useApi@http://localhost:5173/src/api.ts:12:9",
      func: "useApi",
      file: "http://localhost:5173/src/api.ts",
      line: 12,
      column: 9,
      mapped_file: "/proj/src/api.ts",
      mapped_line: 11,
      mapped_column: 4,
      pkg: {
        name: "@tanstack/react-router",
        version: "1.130.2",
        path: "/proj/node_modules/@tanstack/react-router",
        dup_paths: [
          "/proj/node_modules/@tanstack/react-router",
          "/proj/node_modules/x/node_modules/@tanstack/react-router",
        ],
      },
    },
  ],
  values: { evaluatingChain: ["api", "sessions", "list"] },
  http: { method: "GET", url: "/api/x", status: 500, body_excerpt: "boom" },
  extra: { hmrRev: 3, href: "http://localhost:5173/" },
  run_id: "test-run-1",
  ts: 1787800000000,
};

const goldenEvent: EventEnvelope = {
  type: "event",
  kind: "hmr-update",
  data: { files: ["/src/App.tsx"], rev: 4, ts: 1787800000500 },
  run_id: "test-run-1",
  ts: 1787800000500,
};

const goldenRun: RunEnvelope = {
  type: "run",
  run_id: "test-run-1",
  pid: 4242,
  port: 5173,
  git_head: "f1e0648aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  lockfile_hash: "ab".repeat(32),
  vite_dep_hash: "cd".repeat(32),
  fingerprint: { tool: "vite-dev", node: "v22.0.0" },
  changed_since_last_run: ["vite-deps"],
  ts: 1787799999000,
};

describe("live round-trip through the real pixel binary", () => {
  test("run, event, and error envelopes are all accepted and queryable", () => {
    const { project, run } = makeSandbox();

    for (const envelope of [goldenRun, goldenEvent, goldenError]) {
      const result = run(["sniper", "report", "--json", "-", "--repo", project], JSON.stringify(envelope));
      expect(result.status).toBe(0);
      expect(result.stderr).toBe("");
    }

    // Error round-trips intact via `sniper last --json`.
    const last = run(["sniper", "last", "--json", "--repo", project]);
    expect(last.status).toBe(0);
    const parsed = JSON.parse(last.stdout) as {
      errors: Array<Record<string, unknown>>;
      cursor: number;
    };
    expect(parsed.errors.length).toBe(1);
    const record = parsed.errors[0];
    expect(record.surface).toBe("browser-rejection");
    expect(record.kind).toBe("TypeError");
    expect(record.message).toBe(goldenError.message);
    expect(record.run_id).toBe("test-run-1");
    const frames = record.frames as Array<Record<string, unknown>>;
    expect(frames[0].mapped_file).toBe("/proj/src/api.ts");
    expect((frames[0].pkg as Record<string, unknown>).dup_paths).toEqual(
      goldenError.frames![0].pkg!.dup_paths,
    );
    expect(record.values).toEqual({ evaluatingChain: ["api", "sessions", "list"] });

    // Run fingerprint round-trips via `sniper env --json`.
    const env = run(["sniper", "env", "--json", "--repo", project]);
    expect(env.status).toBe(0);
    const envParsed = JSON.parse(env.stdout) as { latest?: Record<string, unknown> };
    const latest = envParsed.latest ?? (JSON.parse(env.stdout) as Record<string, unknown>);
    const runRow = (latest.run_id ? latest : (latest.run as Record<string, unknown>)) ?? latest;
    expect(JSON.stringify(envParsed)).toContain("test-run-1");
    expect(JSON.stringify(envParsed)).toContain(goldenRun.lockfile_hash!);
    void runRow;

    // HMR event round-trips via `sniper hmr --json`.
    const hmr = run(["sniper", "hmr", "--json", "--repo", project]);
    expect(hmr.status).toBe(0);
    expect(hmr.stdout).toContain("hmr-update");
    expect(hmr.stdout).toContain("/src/App.tsx");
  });

  test("dedup: the same error envelope twice yields one row with count 2", () => {
    const { project, run } = makeSandbox();
    const first = run(["sniper", "report", "--json", "-", "--repo", project], JSON.stringify(goldenError));
    const second = run(["sniper", "report", "--json", "-", "--repo", project], JSON.stringify(goldenError));
    expect(JSON.parse(first.stdout).deduped).toBe(false);
    const parsed = JSON.parse(second.stdout) as { deduped: boolean; count: number };
    expect(parsed.deduped).toBe(true);
    expect(parsed.count).toBe(2);
  });

  test("unknown surface is rejected by the Rust parser (contract guard)", () => {
    const { project, run } = makeSandbox();
    const result = run(
      ["sniper", "report", "--json", "-", "--repo", project],
      JSON.stringify({ surface: "not-a-surface", message: "x" }),
    );
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("bad error record");
  });
});
