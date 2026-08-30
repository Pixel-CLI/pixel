import { describe, expect, test } from "bun:test";
import { EventEmitter } from "node:events";
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SinkReporter, resolvePixelBin } from "../src/report.ts";

describe("resolvePixelBin", () => {
  test("explicit path wins when executable", () => {
    const dir = mkdtempSync(join(tmpdir(), "sniper-bin-"));
    const bin = join(dir, "pixel");
    writeFileSync(bin, "#!/bin/sh\nexit 0\n");
    chmodSync(bin, 0o755);
    expect(resolvePixelBin(bin)).toBe(bin);
  });

  test("explicit missing path throws a clear error", () => {
    expect(() => resolvePixelBin("/nope/definitely/missing/pixel")).toThrow(
      /not found at .*missing\/pixel/,
    );
  });

  test("PIXEL_BIN env is honored", () => {
    const dir = mkdtempSync(join(tmpdir(), "sniper-bin-"));
    const bin = join(dir, "pixel-env");
    writeFileSync(bin, "#!/bin/sh\nexit 0\n");
    chmodSync(bin, 0o755);
    const prev = process.env.PIXEL_BIN;
    process.env.PIXEL_BIN = bin;
    try {
      expect(resolvePixelBin()).toBe(bin);
    } finally {
      if (prev === undefined) delete process.env.PIXEL_BIN;
      else process.env.PIXEL_BIN = prev;
    }
  });

  test("bare name resolves via PATH", () => {
    const dir = mkdtempSync(join(tmpdir(), "sniper-path-"));
    mkdirSync(dir, { recursive: true });
    const bin = join(dir, "pixel-on-path");
    writeFileSync(bin, "#!/bin/sh\nexit 0\n");
    chmodSync(bin, 0o755);
    const prevPath = process.env.PATH;
    process.env.PATH = `${dir}:${prevPath}`;
    try {
      expect(resolvePixelBin("pixel-on-path")).toBe(bin);
    } finally {
      process.env.PATH = prevPath;
    }
  });
});

const makeFakeSpawn = (log: { started: string[]; concurrent: number; maxConcurrent: number }) => {
  return ((_bin: string, _args: string[]) => {
    const child = new EventEmitter() as EventEmitter & { stdin: any; stderr: any };
    let body = "";
    child.stderr = new EventEmitter();
    child.stdin = {
      on: () => {},
      end: (data: string) => {
        body = data;
        log.concurrent += 1;
        log.maxConcurrent = Math.max(log.maxConcurrent, log.concurrent);
        setTimeout(() => {
          log.started.push(body);
          log.concurrent -= 1;
          child.emit("close", 0);
        }, 5);
      },
    };
    return child;
  }) as any;
};

describe("SinkReporter", () => {
  test("serializes shell-outs: one child at a time, in order", async () => {
    const log = { started: [] as string[], concurrent: 0, maxConcurrent: 0 };
    const reporter = new SinkReporter({
      bin: "fake",
      repo: ".",
      spawnImpl: makeFakeSpawn(log),
    });
    reporter.report({ surface: "reported", message: "one" });
    reporter.report({ surface: "reported", message: "two" });
    reporter.report({ type: "event", kind: "hmr-update", data: { files: ["a.ts"] } });
    await reporter.flush();
    expect(log.maxConcurrent).toBe(1);
    expect(log.started.map((s) => JSON.parse(s).message ?? JSON.parse(s).kind)).toEqual([
      "one",
      "two",
      "hmr-update",
    ]);
  });

  test("spawn failure never throws and rate-limits warnings", async () => {
    const warnings: string[] = [];
    const reporter = new SinkReporter({
      bin: "fake",
      repo: ".",
      onError: (m) => warnings.push(m),
      spawnImpl: (() => {
        const child = new EventEmitter() as EventEmitter & { stdin: any; stderr: any };
        child.stderr = new EventEmitter();
        child.stdin = {
          on: () => {},
          end: () => setTimeout(() => child.emit("error", new Error("boom")), 1),
        };
        return child;
      }) as any,
    });
    for (let i = 0; i < 10; i++) reporter.report({ surface: "reported", message: `m${i}` });
    await reporter.flush();
    expect(warnings.length).toBe(5); // rate-limited at 5
  });
});
