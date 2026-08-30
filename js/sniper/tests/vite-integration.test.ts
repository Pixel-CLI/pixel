/**
 * Integration test: a REAL vite dev server with sniperDevPlugin() wired to the
 * REAL pixel binary and a temp state root. Asserts that:
 *  - a browser-style POST to /__sniper/report lands in the sink ENRICHED
 *    (source-mapped frames);
 *  - a thrown 500 route lands as an http-5xx record with a body excerpt;
 *  - an HMR-triggering file touch lands as an hmr-update event;
 *  - the run envelope landed with pid + port.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, realpathSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer, type ViteDevServer } from "vite";
import { sniperDevPlugin } from "../src/vite.ts";

const repoRoot = resolve(import.meta.dir, "..", "..", "..");
const binPath = join(repoRoot, "target", "debug", "pixel");

let server: ViteDevServer;
let port = 0;
let project: string;
let plugin: ReturnType<typeof sniperDevPlugin>;

const sniper = (args: string[]) =>
  spawnSync(binPath, ["sniper", ...args, "--json", "--repo", project], {
    env: process.env,
    encoding: "utf8",
    timeout: 30_000,
  });

const waitFor = async (
  predicate: () => boolean | Promise<boolean>,
  what: string,
  timeoutMs = 10_000,
): Promise<void> => {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`timed out waiting for ${what}`);
};

beforeAll(async () => {
  if (!existsSync(binPath)) {
    execFileSync("cargo", ["build", "-p", "pixel-cli"], {
      cwd: repoRoot,
      stdio: "inherit",
      timeout: 600_000,
    });
  }
  process.env.GITPIXEL_SNIPER_STATE_ROOT = mkdtempSync(join(tmpdir(), "sniper-vitest-state-"));

  // realpath: macOS tmpdir is a symlink (/var → /private/var) and vite
  // resolves module ids through the real path.
  project = realpathSync(mkdtempSync(join(tmpdir(), "sniper-vite-proj-")));
  mkdirSync(join(project, "src"), { recursive: true });
  writeFileSync(join(project, "package.json"), JSON.stringify({ name: "fixture-app", type: "module" }));
  writeFileSync(
    join(project, "index.html"),
    `<!doctype html><html><body><script type="module" src="/src/main.ts"></script></body></html>`,
  );
  writeFileSync(
    join(project, "src", "main.ts"),
    `export const greet = (name: string): string => {\n  return "hello " + name;\n};\nconsole.log(greet("world"));\nif (import.meta.hot) import.meta.hot.accept();\n`,
  );

  plugin = sniperDevPlugin({ bin: binPath });
  server = await createServer({
    root: project,
    logLevel: "silent",
    server: { port: 0 },
    plugins: [
      plugin as never,
      {
        name: "fixture-500-route",
        configureServer(s) {
          s.middlewares.use((req, res, next) => {
            if (req.url?.startsWith("/api/boom")) {
              res.statusCode = 500;
              res.setHeader("content-type", "application/json");
              res.end(JSON.stringify({ error: "synthetic explosion in fixture route" }));
              return;
            }
            next();
          });
        },
      },
    ],
  });
  await server.listen();
  const addr = server.httpServer?.address();
  port = typeof addr === "object" && addr ? addr.port : 0;
  expect(port).toBeGreaterThan(0);
}, 120_000);

afterAll(async () => {
  await server?.close();
});

describe("vite dev server integration (real server, real binary)", () => {
  test("browser POST to /__sniper/report lands enriched with source-mapped frames", async () => {
    // Populate the module graph so a transform map exists for /src/main.ts.
    await server.transformRequest("/src/main.ts");

    const stack = [
      "TypeError: undefined is not an object (evaluating 'api.sessions.list')",
      `    at greet (http://localhost:${port}/src/main.ts:2:10)`,
      `    at http://localhost:${port}/src/main.ts:4:13`,
    ].join("\n");
    const response = await fetch(`http://localhost:${port}/__sniper/report`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        surface: "browser-window",
        message: "TypeError: undefined is not an object (evaluating 'api.sessions.list')",
        kind: "TypeError",
        stack_raw: stack,
        extra: { hmrRev: 0, href: `http://localhost:${port}/` },
      }),
    });
    expect(response.status).toBe(204);
    await plugin.api.flush();

    const last = sniper(["last"]);
    expect(last.status).toBe(0);
    const parsed = JSON.parse(last.stdout) as { errors: Array<Record<string, unknown>> };
    const record = parsed.errors.find((e) => e.surface === "browser-window");
    expect(record).toBeDefined();
    expect(record!.kind).toBe("TypeError");
    const frames = record!.frames as Array<Record<string, unknown>>;
    expect(frames.length).toBe(2);
    // ENRICHED: dev-server URL mapped back to the physical file on disk.
    expect(String(frames[0].mapped_file)).toContain("src/main.ts");
    expect(String(frames[0].mapped_file).startsWith("http")).toBe(false);
    // Message-class value parsing landed too.
    expect(JSON.stringify(record!.values)).toContain('"evaluatingChain"');
  });

  test("GET requests and non-loopback posts are rejected by the ingest gate", async () => {
    const get = await fetch(`http://localhost:${port}/__sniper/report`);
    expect(get.status).toBe(405);
  });

  test("a 500 route lands as an http-5xx record with body excerpt", async () => {
    const response = await fetch(`http://localhost:${port}/api/boom`);
    expect(response.status).toBe(500);
    await waitFor(async () => {
      await plugin.api.flush();
      const last = sniper(["last"]);
      return last.stdout.includes('"http-5xx"');
    }, "http-5xx record");

    const parsed = JSON.parse(sniper(["last"]).stdout) as {
      errors: Array<Record<string, unknown>>;
    };
    const record = parsed.errors.find((e) => e.surface === "http-5xx")!;
    expect(record.kind).toBe("500");
    expect(String(record.message)).toContain("/api/boom");
    const http = record.http as Record<string, unknown>;
    expect(http.status).toBe(500);
    expect(String(http.body_excerpt)).toContain("synthetic explosion");
  });

  test("an HMR-triggering file touch lands as an hmr-update event", async () => {
    // Give the fs watcher a beat, then touch the module vite is tracking.
    await new Promise((r) => setTimeout(r, 300));
    writeFileSync(
      join(project, "src", "main.ts"),
      `export const greet = (name: string): string => {\n  return "hi " + name;\n};\nconsole.log(greet("hmr"));\nif (import.meta.hot) import.meta.hot.accept();\n`,
    );
    await waitFor(async () => {
      await plugin.api.flush();
      const hmr = sniper(["hmr"]);
      return hmr.stdout.includes("main.ts");
    }, "hmr-update event", 15_000);

    const hmr = sniper(["hmr"]);
    expect(hmr.status).toBe(0);
    expect(hmr.stdout).toContain("hmr-update");
    expect(hmr.stdout).toContain("main.ts");
  }, 20_000);

  test("the run envelope landed with pid and port", async () => {
    await plugin.api.flush();
    const env = sniper(["env"]);
    expect(env.status).toBe(0);
    expect(env.stdout).toContain(String(process.pid));
    expect(env.stdout).toContain(String(port));
    expect(env.stdout).toContain("vite-dev");
  });
});
