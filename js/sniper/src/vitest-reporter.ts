/**
 * Vitest reporter for the sniper sink: ONE record per failing run
 * (`"2 failed | 10 passed (3.1s)"` + structured failures capped at 50),
 * a `test-pass` event when green. Gated off under CI.
 *
 * Usage: `reporters: ['default', new SniperReporter()]`.
 */

import { spawnSync } from "node:child_process";
import { resolvePixelBin, type ErrorEnvelope, type EventEnvelope } from "./report.ts";

export interface SniperReporterOptions {
  /** Path to the pixel binary. Default: $PIXEL_BIN, then PATH lookup. */
  bin?: string;
  /** Repo passed as `--repo`. Default: process.cwd(). */
  repo?: string;
  /** Injectable transport for tests; defaults to a synchronous shell-out. */
  shipImpl?: (envelope: ErrorEnvelope | EventEnvelope) => void;
}

export interface SniperFailure {
  test: string;
  file?: string;
  line?: number;
  expected?: string;
  received?: string;
  message?: string;
}

const MAX_FAILURES = 50;

/** Minimal structural view of vitest's File/Task tree (v1–v3 compatible). */
interface TaskLike {
  type?: string;
  name?: string;
  filepath?: string;
  file?: { filepath?: string };
  location?: { line?: number };
  tasks?: TaskLike[];
  result?: {
    state?: string;
    duration?: number;
    errors?: Array<{
      message?: string;
      expected?: unknown;
      actual?: unknown;
      stack?: string;
    }>;
  };
}

const asDisplay = (value: unknown): string | undefined => {
  if (value === undefined) return undefined;
  try {
    const encoded = typeof value === "string" ? value : JSON.stringify(value);
    return encoded === undefined ? String(value) : encoded.slice(0, 512);
  } catch {
    return String(value).slice(0, 512);
  }
};

const lineFromStack = (stack: string | undefined, file: string | undefined): number | undefined => {
  if (!stack || !file) return undefined;
  const escaped = file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const m = new RegExp(`${escaped}:(\\d+):\\d+`).exec(stack);
  return m ? Number(m[1]) : undefined;
};

export const collectRunSummary = (
  files: TaskLike[],
): { failed: number; passed: number; durationMs: number; failures: SniperFailure[] } => {
  let failed = 0;
  let passed = 0;
  let durationMs = 0;
  const failures: SniperFailure[] = [];
  const walk = (task: TaskLike, filepath: string | undefined, path: string[]): void => {
    const file = task.filepath ?? task.file?.filepath ?? filepath;
    const nextPath = task.type === "test" || task.type === "suite" || task.name
      ? [...path, task.name ?? ""]
      : path;
    if (task.type === "test") {
      const state = task.result?.state;
      if (state === "fail") {
        failed += 1;
        const err = task.result?.errors?.[0];
        failures.push({
          test: nextPath.filter(Boolean).join(" > "),
          file,
          line: task.location?.line ?? lineFromStack(err?.stack, file),
          expected: asDisplay(err?.expected),
          received: asDisplay(err?.actual),
          message: err?.message?.slice(0, 1024),
        });
      } else if (state === "pass") {
        passed += 1;
      }
      durationMs += task.result?.duration ?? 0;
      return;
    }
    for (const child of task.tasks ?? []) walk(child, file, nextPath);
  };
  for (const file of files) {
    for (const child of file.tasks ?? []) {
      walk(child, file.filepath ?? file.file?.filepath, []);
    }
  }
  return { failed, passed, durationMs, failures };
};

export class SniperReporter {
  private readonly opts: SniperReporterOptions;

  constructor(opts: SniperReporterOptions = {}) {
    this.opts = opts;
  }

  onFinished(files: TaskLike[] = [], _errors: unknown[] = []): void {
    try {
      if (process.env.CI) return;
      const { failed, passed, durationMs, failures } = collectRunSummary(files);
      const seconds = (durationMs / 1000).toFixed(1);
      const ship = this.opts.shipImpl ?? ((envelope) => this.shell(envelope));
      if (failed > 0) {
        const capped = failures.slice(0, MAX_FAILURES);
        ship({
          surface: "vitest",
          kind: "test-failures",
          message: `${failed} failed | ${passed} passed (${seconds}s)`,
          extra: {
            failures: capped,
            ...(failures.length > MAX_FAILURES
              ? { truncatedCount: failures.length - MAX_FAILURES }
              : {}),
          },
          ts: Date.now(),
        });
      } else {
        ship({
          type: "event",
          kind: "test-pass",
          data: { passed, durationMs: Math.round(durationMs) },
          ts: Date.now(),
        });
      }
    } catch {
      /* the sink must never break the test run */
    }
  }

  private shell(envelope: ErrorEnvelope | EventEnvelope): void {
    try {
      const bin = resolvePixelBin(this.opts.bin);
      const repo = this.opts.repo ?? process.cwd();
      const result = spawnSync(bin, ["sniper", "report", "--json", "-", "--repo", repo], {
        input: JSON.stringify(envelope),
        stdio: ["pipe", "ignore", "pipe"],
        timeout: 10_000,
      });
      if (result.status !== 0) {
        console.warn(
          `[sniper] vitest reporter shell-out failed${
            result.stderr ? `: ${String(result.stderr).trim()}` : ""
          }`,
        );
      }
    } catch (err) {
      console.warn(`[sniper] vitest reporter failed: ${String(err)}`);
    }
  }
}

export default SniperReporter;
