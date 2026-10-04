// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Helpers get repo-unique names (`run` would collide with the Rust `run` hook
// entries and widen the graph's same-name lower bound).
const root = mkdtempSync(join(tmpdir(), "pixel-codex-ab-"));
const repo = join(root, "repo");
const bin = join(root, "bin");
mkdirSync(repo); mkdirSync(bin);
const runCmd = (args: string[]) => Bun.spawnSync(args, { cwd: repo, stdout: "pipe", stderr: "pipe" });
if (runCmd(["git", "init", "-q"]).exitCode !== 0) throw new Error("git init failed");
runCmd(["git", "config", "user.email", "test@example.com"]); runCmd(["git", "config", "user.name", "Test"]);
writeFileSync(join(repo, "README.md"), "fixture\n");
runCmd(["git", "add", "README.md"]); runCmd(["git", "commit", "-qm", "fixture"]);
const fakeBin = (name: string, body: string) => { const path = join(bin, name); writeFileSync(path, `#!/usr/bin/env bash\nset -e\n${body}\n`); runCmd(["chmod", "+x", path]); };
fakeBin("pixel", "case \"$1\" in install|build-index) exit 0;; classify) printf '%s\\n' '{\"marker\":\"complete\",\"predicted\":\"pixel\",\"probs\":{\"pixel\":0.8,\"none\":0.1,\"native\":0.1}}'; exit 0;; esac");
fakeBin("codex", "[ \"${PIXEL_POLICY:-}\" = enforce ] && printf '%s\\n' '{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\",\"command\":\"pixel find-code\"}}'\nfor arg in \"$@\"; do [ \"$arg\" = --output-last-message ] && next=1 || { [ \"${next:-}\" = 1 ] && { printf answer > \"$arg\"; break; }; }; done");
fakeBin("tmux", "case \"$1\" in has-session) exit 1;; new-session|split-window) last=\"${!#}\"; bash -c \"$last\" >/dev/null 2>&1 & exit 0;; select-layout) exit 0;; esac");
const script = join(process.cwd(), "scripts/codex-pixel-ab.sh");
const result = Bun.spawnSync([script, "--repo", repo, "--prompt", "Does Pixel help?", "--pixel-bin", join(bin, "pixel"), "--pixel-policy", "classify", "--classify-min-confidence", "0.60", "--session", "stub-ab", "--detach", "--timeout", "3"], { cwd: repo, env: { ...process.env, PATH: `${bin}:${process.env.PATH}` }, stdout: "pipe", stderr: "pipe" });
if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
const reportPath = new TextDecoder().decode(result.stdout).match(/A\/B report: (.+)/)?.[1]?.trim();
if (!reportPath) throw new Error("report path missing");
const report = readFileSync(reportPath, "utf8");
if (!report.includes("Raw Codex") || !report.includes("Codex + Pixel") || !report.includes("Requested Pixel policy: `classify`") || !report.includes("Effective Pixel policy: `enforce`") || !report.includes("Classifier route / winning probability: `pixel` / `0.8`") || !report.includes("| Codex + Pixel | 0 | 0s | 1 |") || report.includes("timeout")) throw new Error("report contract missing");
rmSync(root, { recursive: true, force: true });
console.log("codex-pixel-ab contract: ok");
