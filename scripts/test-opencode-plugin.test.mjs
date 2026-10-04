// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// The V2 OpenCode route-tool adapter and V1 guard fallback, exercised without
// a model or editor UI. Run with: bun test scripts/test-opencode-plugin.test.mjs
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
import { describe, expect, test } from "bun:test";

const scratch = mkdtempSync(join(tmpdir(), "pixel-opencode-route-"));
const repo = join(scratch, "repo");
const pixel = join(scratch, "pixel");
const source = readFileSync(new URL("../crates/pixel-install/assets/opencode-pixel.js", import.meta.url), "utf8")
  .replace("__PIXEL_BIN__", JSON.stringify(pixel));
const pluginPath = join(scratch, "opencode-pixel.js");
writeFileSync(pluginPath, source);
const { default: plugin, PixelGuardPlugin } = await import(pathToFileURL(pluginPath).href);

const makeRouteRunner = () => {
  const calls = [];
  const route = {
    task: "inspect route adapter",
    steps: [
      { order: 1, command: "pixel find-code behavior" },
      { order: 2, command: "rtk sed -n 20,60p src/lib.rs" },
    ],
    fallback: "use native search after two unhelpful Pixel calls",
    validation: ["run focused test"],
  };
  const run = (binary, args, options) => {
    calls.push({ binary, args, options });
    return {
      status: 0,
      stdout: JSON.stringify({ ...route, task: args[1] }),
      stderr: options.env.PIXEL_METRICS === "0"
        ? ""
        : "🟩 pixel · execution-brief · #a1b2c3\n└──────┘",
    };
  };
  return { run, calls };
};

const mockV2 = () => {
  let registered;
  let disposed = 0;
  return {
    ctx: {
      location: { directory: repo },
      tool: {
        transform(callback) {
          callback({ add: (tool) => { registered = tool; } });
          return { dispose: () => { disposed++; } };
        },
      },
    },
    tool: () => registered,
    disposed: () => disposed,
  };
};

describe.serial("OpenCode Pixel execution route", () => {
  test("V2 plugin exposes one route action and disposes its registration", async () => {
    expect(plugin.id).toBe("pixel");
    expect(typeof plugin.setup).toBe("function");
    expect(typeof plugin.server).toBe("function");
    const m = mockV2();
    const cleanup = await plugin.setup(m.ctx);
    expect(m.tool().name).toBe("pixel");
    expect(m.tool().input.properties.action.enum).toEqual(["execution_brief"]);
    expect(m.tool().input.required).toEqual(["action", "task"]);
    cleanup();
    expect(m.disposed()).toBe(1);
  });

  test("route output is ordered and the exact invocation metric is a separate item", async () => {
    const m = mockV2();
    const fake = makeRouteRunner();
    await plugin.setup(m.ctx, fake.run);
    const result = await m.tool().execute(
      { action: "execution_brief", task: "inspect route adapter" },
      { directory: repo, sessionID: "session-a" },
    );
    expect(result.content).toHaveLength(2);
    expect(fake.calls[0].args).toEqual(["execution-brief", "inspect route adapter", "--json"]);
    expect(fake.calls[0].options.cwd).toBe(repo);
    const payload = JSON.parse(result.content[0].text);
    expect(payload.action).toBe("execution_brief");
    expect(payload.route.steps.map((step) => step.order)).toEqual([1, 2]);
    expect(payload.route.steps[0].command).toBe("pixel find-code behavior");
    expect(payload.route.steps[1].command).toBe("rtk sed -n 20,60p src/lib.rs");
    expect(payload.route.fallback).toContain("two unhelpful Pixel calls");
    expect(result.content[1]).toEqual({
      type: "text",
      text: "🟩 pixel · execution-brief · #a1b2c3\n└──────┘",
    });
  });

  test("PIXEL_METRICS=0 suppresses the metrics item without changing route data", async () => {
    process.env.PIXEL_METRICS = "0";
    try {
      const m = mockV2();
      const fake = makeRouteRunner();
      await plugin.setup(m.ctx, fake.run);
      const result = await m.tool().execute(
        { action: "execution_brief", task: "quiet route" },
        { directory: repo },
      );
      expect(result.content).toHaveLength(1);
      expect(fake.calls[0].options.env.PIXEL_METRICS).toBe("0");
      expect(JSON.parse(result.content[0].text).route.steps).toHaveLength(2);
    } finally {
      delete process.env.PIXEL_METRICS;
    }
  });

  test("missing Pixel and invalid tool input return native fallback instead of throwing", async () => {
    const m = mockV2();
    await plugin.setup(m.ctx, () => ({ error: new Error("spawn failed"), status: null }));
    const tool = m.tool();
    const invalid = await tool.execute({ action: "execution_brief", task: " " }, { directory: repo });
    expect(JSON.parse(invalid.content[0].text).error).toContain("requires a task");

    const result = await tool.execute(
      { action: "execution_brief", task: "test fallback" },
      { directory: repo },
    );
    const payload = JSON.parse(result.content[0].text);
    expect(payload.error).toBeDefined();
    expect(payload.fallback).toContain("continue with native tools");
  });

  test("V1 guard remains fail-open when Pixel cannot be spawned", async () => {
    const hooks = await PixelGuardPlugin({ directory: repo });
    const input = { tool: "read" };
    const output = { args: { filePath: "src/lib.rs" } };
    await expect(hooks["tool.execute.before"](input, output)).resolves.toBeUndefined();
    expect(output.args.filePath).toBe("src/lib.rs");
  });
});

describe("installed OpenCode version", () => {
  test("reports the live CLI version when it is available", () => {
    const version = spawnSync("opencode", ["--version"], { encoding: "utf8" });
    if (version.error) return;
    expect(version.status).toBe(0);
    expect(version.stdout.trim()).toMatch(/^opencode v\d+\./);
  });
});
