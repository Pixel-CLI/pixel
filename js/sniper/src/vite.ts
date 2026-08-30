/**
 * `sniperDevPlugin()` — one import in any vite config wires every server-side
 * capture surface into the pixel sniper sink:
 *
 * - a `run` envelope on server start (pid, port, git HEAD, lockfile hash,
 *   .vite deps metadata hash);
 * - process-level uncaughtExceptionMonitor + unhandledRejection (record, never
 *   swallow — default crash semantics are preserved);
 * - dev-only `POST /__sniper/report` ingest middleware (loopback-gated) that
 *   enriches browser payloads (source-mapped frames + package provenance);
 * - HTTP 5xx capture (4KB body excerpt, 499 skipped);
 * - `server.ws.send` wrap: vite error payloads → `vite-transform` records,
 *   update/full-reload payloads → lifecycle events;
 * - fs.watch on `node_modules/.vite/deps/_metadata.json` → `dep-optimized`.
 *
 * Every capture path is try/catch + reentrancy-guarded: the sink must never
 * break the dev server. Records ship serially through one child process at a
 * time (`pixel sniper report --json -`).
 */

import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { existsSync, readFileSync, watch, type FSWatcher } from "node:fs";
import { join } from "node:path";
import type { IncomingMessage, ServerResponse } from "node:http";
import {
  enrichFrames,
  parseEvaluatingChain,
  parseStack,
  ProvenanceTracker,
  type DevServerLike,
} from "./enrich.ts";
import {
  resolvePixelBin,
  SinkReporter,
  type ErrorEnvelope,
  type EventKind,
  type Surface,
} from "./report.ts";

export interface SniperDevPluginOptions {
  /** Path to the pixel binary. Default: $PIXEL_BIN, then PATH lookup. */
  bin?: string;
  /** Ingest endpoint path. Default: /__sniper/report. */
  endpoint?: string;
}

const BROWSER_SURFACES: ReadonlySet<string> = new Set([
  "browser-window",
  "browser-rejection",
  "error-boundary",
  "browser-console",
  "reported",
]);

const sha256File = (path: string): string | undefined => {
  try {
    if (!existsSync(path)) return undefined;
    return createHash("sha256").update(readFileSync(path)).digest("hex");
  } catch {
    return undefined;
  }
};

const gitHead = (root: string): string | undefined => {
  try {
    return execFileSync("git", ["-C", root, "rev-parse", "HEAD"], {
      encoding: "utf8",
      timeout: 5000,
      stdio: ["ignore", "pipe", "ignore"],
    }).trim();
  } catch {
    return undefined;
  }
};

const isLoopback = (remoteAddress: string | undefined): boolean => {
  if (!remoteAddress) return false;
  return (
    remoteAddress === "127.0.0.1" ||
    remoteAddress === "::1" ||
    remoteAddress === "::ffff:127.0.0.1" ||
    remoteAddress.startsWith("127.")
  );
};

const readBody = (req: IncomingMessage, cap: number): Promise<string> =>
  new Promise((resolve, reject) => {
    let body = "";
    req.on("data", (chunk: Buffer) => {
      body += String(chunk);
      if (body.length > cap) {
        reject(new Error("payload too large"));
        req.destroy();
      }
    });
    req.on("end", () => resolve(body));
    req.on("error", reject);
  });

/** Structural slice of ViteDevServer (kept loose so vite stays a peer dep). */
interface ViteServerish extends DevServerLike {
  config: { root: string; server?: { port?: number } };
  httpServer?: {
    once(event: string, cb: () => void): void;
    address(): { port?: number } | string | null;
  } | null;
  middlewares: {
    use(
      fn: (req: IncomingMessage, res: ServerResponse, next: (err?: unknown) => void) => void,
    ): void;
  };
  ws?: { send?: (...args: unknown[]) => unknown };
  hot?: { send?: (...args: unknown[]) => unknown };
  environments?: DevServerLike["environments"] & {
    client?: { hot?: { send?: (...args: unknown[]) => unknown } };
  };
}

export const sniperDevPlugin = (options: SniperDevPluginOptions = {}) => {
  const endpoint = options.endpoint ?? "/__sniper/report";
  let reporter: SinkReporter | undefined;

  return {
    name: "pixel-sniper",
    apply: "serve" as const,
    api: {
      /** Test hook: resolves once every queued record has been shipped. */
      flush: (): Promise<void> => reporter?.flush() ?? Promise.resolve(),
    },

    configureServer(server: ViteServerish) {
      // (a) Resolve the binary up front — a broken install should be loud.
      const bin = resolvePixelBin(options.bin);
      const root = server.config.root;
      reporter = new SinkReporter({ bin, repo: root });
      const sink = reporter;
      const runId = `${Date.now().toString(36)}-${randomBytes(4).toString("hex")}`;
      const provenance = new ProvenanceTracker();
      const metadataPath = join(root, "node_modules", ".vite", "deps", "_metadata.json");
      let capturing = false;
      let hmrRev = 0;

      const guarded = (fn: () => void): void => {
        if (capturing) return;
        capturing = true;
        try {
          fn();
        } catch {
          /* the sink must never break the dev server */
        } finally {
          capturing = false;
        }
      };

      const recordError = (envelope: Omit<ErrorEnvelope, "run_id" | "ts">): void => {
        guarded(() => sink.report({ ...envelope, run_id: runId, ts: Date.now() }));
      };

      const recordEvent = (kind: EventKind, data?: unknown): void => {
        guarded(() =>
          sink.report({ type: "event", kind, data, run_id: runId, ts: Date.now() }),
        );
      };

      // (b) Run envelope: emitted once the port is known (or immediately in
      // middleware mode where no http server exists).
      const emitRun = (port?: number): void => {
        guarded(() => {
          const lockfile = ["bun.lock", "package-lock.json", "bun.lockb"]
            .map((name) => join(root, name))
            .find(existsSync);
          sink.report({
            type: "run",
            run_id: runId,
            pid: process.pid,
            port,
            git_head: gitHead(root),
            lockfile_hash: lockfile ? sha256File(lockfile) : undefined,
            vite_dep_hash: sha256File(metadataPath),
            fingerprint: { tool: "vite-dev", node: process.version },
            ts: Date.now(),
          });
        });
      };
      if (server.httpServer) {
        server.httpServer.once("listening", () => {
          const addr = server.httpServer?.address();
          emitRun(typeof addr === "object" && addr ? addr.port : server.config.server?.port);
        });
      } else {
        emitRun(server.config.server?.port);
      }

      // (c) Process-level capture: monitor observes without altering crash
      // semantics; the rejection listener records then re-emits the default
      // behavior (rethrow) when nobody else is listening — never swallow.
      const onUncaught = (err: unknown): void => {
        recordError({ surface: "node-uncaught", ...errorFieldsNode(err) });
      };
      const onUnhandled = (reason: unknown): void => {
        recordError({ surface: "node-unhandled", ...errorFieldsNode(reason) });
        if (process.listenerCount("unhandledRejection") === 1) {
          // We are the only listener; without us node would have crashed.
          // Re-emit default semantics instead of silently swallowing.
          throw reason;
        }
      };
      process.on("uncaughtExceptionMonitor", onUncaught);
      process.on("unhandledRejection", onUnhandled);
      server.httpServer?.once("close", () => {
        // Loose call shape: bun's process typings narrow removeListener's
        // event names differently from @types/node.
        const proc = process as unknown as {
          removeListener(event: string, listener: (...args: never[]) => void): void;
        };
        proc.removeListener("uncaughtExceptionMonitor", onUncaught);
        proc.removeListener("unhandledRejection", onUnhandled);
        metadataWatcher?.close();
      });

      // (e) 5xx capture — registered before other middlewares (configureServer
      // direct registrations run ahead of vite internals).
      server.middlewares.use((req, res, next) => {
        try {
          const chunks: Buffer[] = [];
          let captured = 0;
          const push = (chunk: unknown): void => {
            try {
              if (captured >= 4096 || chunk == null) return;
              const buf =
                typeof chunk === "string"
                  ? Buffer.from(chunk)
                  : Buffer.isBuffer(chunk)
                    ? chunk
                    : undefined;
              if (!buf) return;
              const take = buf.subarray(0, 4096 - captured);
              chunks.push(take);
              captured += take.length;
            } catch {
              /* never break the response */
            }
          };
          const originalWrite = res.write.bind(res);
          const originalEnd = res.end.bind(res);
          res.write = ((chunk: unknown, ...rest: unknown[]) => {
            push(chunk);
            return (originalWrite as (...a: unknown[]) => boolean)(chunk, ...rest);
          }) as typeof res.write;
          res.end = ((chunk?: unknown, ...rest: unknown[]) => {
            push(chunk);
            return (originalEnd as (...a: unknown[]) => ServerResponse)(chunk, ...rest);
          }) as typeof res.end;
          res.on("finish", () => {
            const status = res.statusCode;
            if (status >= 500 && status !== 499) {
              const excerpt = Buffer.concat(chunks).toString("utf8");
              recordError({
                surface: "http-5xx",
                kind: String(status),
                message: `${req.method ?? "GET"} ${req.url ?? "?"} -> ${status}`,
                http: {
                  method: req.method,
                  url: req.url,
                  status,
                  body_excerpt: excerpt || undefined,
                },
              });
            }
          });
        } catch {
          /* never break the dev server */
        }
        next();
      });

      // (d) Dev-only ingest endpoint, loopback-gated.
      server.middlewares.use((req, res, next) => {
        if (!req.url || req.url.split("?")[0] !== endpoint) return next();
        if (req.method !== "POST") {
          res.statusCode = 405;
          res.end();
          return;
        }
        if (!isLoopback(req.socket?.remoteAddress)) {
          res.statusCode = 403;
          res.end();
          return;
        }
        void (async () => {
          try {
            const body = await readBody(req, 256 * 1024);
            const payload = JSON.parse(body) as {
              surface?: string;
              message?: string;
              kind?: string;
              stack_raw?: string;
              values?: unknown;
              extra?: Record<string, unknown>;
            };
            const surface: Surface = BROWSER_SURFACES.has(payload.surface ?? "")
              ? (payload.surface as Surface)
              : "reported";
            const frames = await enrichFrames(
              parseStack(payload.stack_raw),
              server,
              provenance,
            );
            const chain = parseEvaluatingChain(payload.message ?? "");
            const values =
              chain || payload.values !== undefined
                ? {
                    ...(typeof payload.values === "object" && payload.values !== null
                      ? (payload.values as Record<string, unknown>)
                      : payload.values !== undefined
                        ? { value: payload.values }
                        : {}),
                    ...(chain ? { evaluatingChain: chain } : {}),
                  }
                : undefined;
            recordError({
              surface,
              message: payload.message ?? "unknown browser error",
              kind: payload.kind,
              stack_raw: payload.stack_raw,
              frames: frames.length > 0 ? frames : undefined,
              values,
              extra: payload.extra,
            });
            res.statusCode = 204;
            res.end();
          } catch {
            res.statusCode = 400;
            res.end();
          }
        })();
      });

      // (f) ws.send wrap: transform errors + HMR lifecycle. Vite may expose
      // several hot channels that delegate to each other; a WeakSet dedupes.
      const seenPayloads = new WeakSet<object>();
      const handleWsPayload = (payload: unknown): void => {
        if (typeof payload !== "object" || payload === null) return;
        if (seenPayloads.has(payload)) return;
        seenPayloads.add(payload);
        const typed = payload as {
          type?: string;
          err?: {
            message?: string;
            stack?: string;
            id?: string;
            plugin?: string;
            frame?: string;
            loc?: { file?: string; line?: number; column?: number };
          };
          updates?: Array<{ path?: string; acceptedPath?: string }>;
        };
        if (typed.type === "error" && typed.err) {
          const err = typed.err;
          const frames = err.loc?.file
            ? [
                {
                  raw: `${err.loc.file}:${err.loc.line ?? 0}:${err.loc.column ?? 0}`,
                  file: err.loc.file,
                  line: err.loc.line,
                  column: err.loc.column,
                  mapped_file: err.loc.file,
                  mapped_line: err.loc.line,
                  mapped_column: err.loc.column,
                },
              ]
            : undefined;
          recordError({
            surface: "vite-transform",
            kind: err.plugin ? `plugin:${err.plugin}` : undefined,
            message: err.message ?? "vite transform error",
            stack_raw: err.stack,
            frames,
            extra: {
              id: err.id,
              plugin: err.plugin,
              frame: err.frame?.slice(0, 2048),
            },
          });
        } else if (typed.type === "update") {
          hmrRev += 1;
          const files = (typed.updates ?? [])
            .map((u) => u.path ?? u.acceptedPath)
            .filter((f): f is string => typeof f === "string");
          recordEvent("hmr-update", { files, rev: hmrRev, ts: Date.now() });
        } else if (typed.type === "full-reload") {
          recordEvent("full-reload", { ts: Date.now() });
        }
      };
      const wrapSend = (target: { send?: (...args: unknown[]) => unknown } | undefined): void => {
        if (!target || typeof target.send !== "function") return;
        const original = target.send.bind(target);
        target.send = (...args: unknown[]) => {
          try {
            // send(payload) or send(event, data) — only the payload form
            // carries error/update objects.
            if (args.length >= 1 && typeof args[0] === "object") handleWsPayload(args[0]);
          } catch {
            /* never break HMR */
          }
          return original(...args);
        };
      };
      wrapSend(server.ws);
      wrapSend(server.hot);
      wrapSend(server.environments?.client?.hot);

      // (g) dep re-optimization watch — the duplicate-module smoking gun.
      let metadataWatcher: FSWatcher | undefined;
      let depDebounce: ReturnType<typeof setTimeout> | undefined;
      const depsDir = join(root, "node_modules", ".vite", "deps");
      try {
        if (existsSync(depsDir)) {
          metadataWatcher = watch(depsDir, (event, filename) => {
            if (filename !== "_metadata.json") return;
            clearTimeout(depDebounce);
            depDebounce = setTimeout(() => {
              recordEvent("dep-optimized", {
                vite_dep_hash: sha256File(metadataPath),
                ts: Date.now(),
              });
            }, 200);
          });
        }
      } catch {
        /* watching is best-effort */
      }
    },
  };
};

const errorFieldsNode = (
  err: unknown,
): { message: string; kind?: string; stack_raw?: string } => {
  if (err instanceof Error) {
    return { message: err.message || String(err), kind: err.name, stack_raw: err.stack };
  }
  if (typeof err === "string") return { message: err };
  try {
    return { message: JSON.stringify(err)?.slice(0, 2000) ?? String(err) };
  } catch {
    return { message: String(err) };
  }
};

export default sniperDevPlugin;
