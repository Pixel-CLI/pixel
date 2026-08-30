/**
 * Shared plumbing for every server-side adapter: the envelope types that
 * mirror the Rust ingest contract (`pixel-sniper/src/types.rs`), pixel
 * binary resolution, and a serialized shell-out queue that pipes one JSON
 * record at a time into `pixel sniper report --json -`.
 *
 * Field names are snake_case on purpose — they must round-trip through the
 * Rust serde structs verbatim.
 */

import { spawn } from "node:child_process";
import { accessSync, constants, existsSync, statSync } from "node:fs";
import { delimiter, isAbsolute, join } from "node:path";

// ---------------------------------------------------------------------------
// Envelope types (mirror crates/pixel-sniper/src/types.rs)
// ---------------------------------------------------------------------------

export type Surface =
  | "browser-window"
  | "browser-rejection"
  | "error-boundary"
  | "browser-console"
  | "server-console"
  | "node-uncaught"
  | "node-unhandled"
  | "http-5xx"
  | "vite-transform"
  | "vitest"
  | "tsc"
  | "run-wrapper"
  | "reported";

export type EventKind =
  | "server-start"
  | "hmr-update"
  | "full-reload"
  | "dep-optimized"
  | "test-pass"
  | "build-ok";

export interface FramePackage {
  name: string;
  version?: string;
  path?: string;
  dup_paths?: string[];
}

export interface Frame {
  raw: string;
  func?: string;
  file?: string;
  line?: number;
  column?: number;
  mapped_file?: string;
  mapped_line?: number;
  mapped_column?: number;
  pkg?: FramePackage;
}

export interface HttpContext {
  method?: string;
  url?: string;
  status?: number;
  body_excerpt?: string;
}

/** An error record — the default envelope (no `type` tag needed). */
export interface ErrorEnvelope {
  type?: "error";
  surface: Surface;
  message: string;
  kind?: string;
  stack_raw?: string;
  frames?: Frame[];
  values?: unknown;
  http?: HttpContext;
  extra?: unknown;
  run_id?: string;
  ts?: number;
}

export interface EventEnvelope {
  type: "event";
  kind: EventKind;
  data?: unknown;
  run_id?: string;
  ts?: number;
}

export interface RunEnvelope {
  type: "run";
  run_id: string;
  pid?: number;
  port?: number;
  git_head?: string;
  lockfile_hash?: string;
  vite_dep_hash?: string;
  fingerprint?: unknown;
  changed_since_last_run?: string[];
  ts?: number;
}

export type ReportEnvelope = ErrorEnvelope | EventEnvelope | RunEnvelope;

// ---------------------------------------------------------------------------
// Binary resolution
// ---------------------------------------------------------------------------

const isExecutableFile = (candidate: string): boolean => {
  try {
    if (!statSync(candidate).isFile()) return false;
    accessSync(candidate, constants.X_OK);
    return true;
  } catch {
    return false;
  }
};

/**
 * Resolve the pixel binary: explicit option > $PIXEL_BIN > `pixel`
 * on PATH. Throws with a clear, actionable message when unresolvable.
 */
export const resolvePixelBin = (explicit?: string): string => {
  const candidate = explicit ?? process.env.PIXEL_BIN;
  if (candidate) {
    if (isAbsolute(candidate) || candidate.includes("/")) {
      if (isExecutableFile(candidate)) return candidate;
      throw new Error(
        `@pixel/sniper: pixel binary not found at ${JSON.stringify(candidate)} ` +
          `(from ${explicit ? "plugin options.bin" : "$PIXEL_BIN"}). ` +
          `Point options.bin or $PIXEL_BIN at an executable pixel build.`,
      );
    }
    const onPath = findOnPath(candidate);
    if (onPath) return onPath;
    throw new Error(
      `@pixel/sniper: ${JSON.stringify(candidate)} is not on PATH. ` +
        `Point options.bin or $PIXEL_BIN at an executable pixel build.`,
    );
  }
  const found = findOnPath("pixel");
  if (found) return found;
  throw new Error(
    "@pixel/sniper: `pixel` was not found on PATH and neither options.bin " +
      "nor $PIXEL_BIN is set. Install pixel (cargo build --release; copy " +
      "target/release/pixel onto PATH) or set PIXEL_BIN=/path/to/pixel.",
  );
};

const findOnPath = (name: string): string | undefined => {
  const pathVar = process.env.PATH ?? "";
  for (const dir of pathVar.split(delimiter)) {
    if (!dir) continue;
    const candidate = join(dir, name);
    if (existsSync(candidate) && isExecutableFile(candidate)) return candidate;
  }
  return undefined;
};

// ---------------------------------------------------------------------------
// Serialized shell-out queue
// ---------------------------------------------------------------------------

export interface SinkReporterOptions {
  bin: string;
  /** Repo path passed as `--repo` so the store resolves to this project. */
  repo: string;
  /** Injectable for tests; defaults to node:child_process spawn. */
  spawnImpl?: typeof spawn;
  /** Called (rate-limited) when a shell-out fails. Defaults to console.warn. */
  onError?: (message: string) => void;
}

/**
 * Queues envelopes and pipes them one at a time (a single child process at a
 * time, strictly ordered) into `pixel sniper report --json -`.
 *
 * Every path is best-effort: a failed shell-out is logged (rate-limited) and
 * dropped — the sink must never break the host process.
 */
export class SinkReporter {
  private readonly opts: SinkReporterOptions;
  private queue: ReportEnvelope[] = [];
  private draining = false;
  private pendingResolvers: Array<() => void> = [];
  private warnCount = 0;

  constructor(opts: SinkReporterOptions) {
    this.opts = opts;
  }

  /** Enqueue one envelope. Never throws. */
  report(envelope: ReportEnvelope): void {
    try {
      this.queue.push(envelope);
      if (!this.draining) void this.drain();
    } catch {
      /* never break the host */
    }
  }

  /** Resolves once everything enqueued so far has been shipped (or dropped). */
  flush(): Promise<void> {
    if (!this.draining && this.queue.length === 0) return Promise.resolve();
    return new Promise((resolve) => {
      this.pendingResolvers.push(resolve);
    });
  }

  private async drain(): Promise<void> {
    this.draining = true;
    try {
      while (this.queue.length > 0) {
        const envelope = this.queue.shift()!;
        await this.ship(envelope);
      }
    } finally {
      this.draining = false;
      const resolvers = this.pendingResolvers;
      this.pendingResolvers = [];
      for (const resolve of resolvers) resolve();
    }
  }

  private ship(envelope: ReportEnvelope): Promise<void> {
    return new Promise((resolve) => {
      try {
        const spawnImpl = this.opts.spawnImpl ?? spawn;
        const child = spawnImpl(
          this.opts.bin,
          ["sniper", "report", "--json", "-", "--repo", this.opts.repo],
          { stdio: ["pipe", "ignore", "pipe"] },
        );
        let stderr = "";
        child.stderr?.on("data", (chunk: Buffer) => {
          if (stderr.length < 2048) stderr += String(chunk);
        });
        child.on("error", (err: Error) => {
          this.warn(`pixel sniper report spawn failed: ${err.message}`);
          resolve();
        });
        child.on("close", (code: number | null) => {
          if (code !== 0) {
            this.warn(
              `pixel sniper report exited ${code}${stderr ? `: ${stderr.trim()}` : ""}`,
            );
          }
          resolve();
        });
        child.stdin?.on("error", () => {
          /* EPIPE on dead child — close handler resolves */
        });
        child.stdin?.end(JSON.stringify(envelope));
      } catch (err) {
        this.warn(`pixel sniper report failed: ${String(err)}`);
        resolve();
      }
    });
  }

  private warn(message: string): void {
    this.warnCount += 1;
    if (this.warnCount > 5) return; // rate-limit: the sink stays quiet after 5 warnings
    try {
      (this.opts.onError ?? ((m: string) => console.warn(`[sniper] ${m}`)))(message);
    } catch {
      /* never break the host */
    }
  }
}
