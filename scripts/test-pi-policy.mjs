// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

// Executable extension contract: bun scripts/test-pi-policy.mjs
// The real extension handles events; only its host and Pixel process are fixtures.
import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = mkdtempSync(join(tmpdir(), "pi-policy-"));
const binary = join(root, "pixel");
const trace = join(root, "calls.jsonl");
const taskTrace = join(root, "tasks.jsonl");
const settingsPath = join(root, "settings.json");
const editedPath = join(root, "edited.txt");
const originalEnv = [process.env.PIXEL_POLICY, process.env.PIXEL_TARGETS_GUARD];
let passed = 0;
const configure = (settings = {}) => writeFileSync(settingsPath, JSON.stringify(settings));
const calls = () => readFileSync(trace, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
const restore = (name, value) => value === undefined ? delete process.env[name] : process.env[name] = value;

try {
  configure();
  writeFileSync(trace, "");
  writeFileSync(taskTrace, "");
  writeFileSync(editedPath, "before");
  // The bash fence's canonical containment check requires an existing
  // repository tree. Read fixtures stay lexical, so an empty `src/`
  // directory is enough for `ls src`, `rg error src` and `cat src/main.rs`
  // to exercise `arg_reads_repo`.
  mkdirSync(join(root, "src"), { recursive: true });
  writeFileSync(join(root, "src/main.rs"), "fn main() {}\n");
  writeFileSync(binary, `#!${process.execPath}
import { appendFileSync, readFileSync } from "node:fs";
const args = process.argv.slice(2);
const settings = JSON.parse(readFileSync(${JSON.stringify(settingsPath)}, "utf8"));
appendFileSync(${JSON.stringify(trace)}, JSON.stringify(args) + "\\n");
if (settings.fail?.includes(args[0])) { console.error("fixture Pixel unavailable: " + args[0]); process.exit(1); }
const operations = ["status", "scope-task", "execution-brief", "repo-state", "review-changes", "commit-history", "find-code", "fetch", "commit", "commit-and-push", "list-areas", "search-content", "impact", "pack-context", "what-changed", "classify"];
const box = "warning: diagnostic line\\n\u{1F7E9} pixel " + args[0] + " \u2740 1.0ms\\n  \u2502\\n  \u2514\u2500\u2500\u2500\\n";
if (!args.includes("off") && !["--version", "--help"].includes(args[0])) process.stderr.write(box);
switch (args[0]) {
  case "run-hook": {
    const payload = JSON.parse(readFileSync(0, "utf8"));
    const event = args[args.indexOf("--event") + 1];
    appendFileSync(${JSON.stringify(taskTrace)}, JSON.stringify({ event, payload }) + "\\n");
    if (settings.taskDelay) await new Promise((done) => setTimeout(done, settings.taskDelay));
    console.log(JSON.stringify(settings.task?.[event] ?? { decision: "allow" }));
    break;
  }
  case "--version": console.log("pixel 0.6.0"); break;
  case "--help": console.log("Commands:\\n" + operations.filter(op => !settings.missing?.includes(op)).map(op => "  " + op + "  Operation").join("\\n")); break;
  case "status": console.log(JSON.stringify({index:{base_files:1},graph:{present:true},facts:{fresh:true}})); break;
  case "config": console.log(JSON.stringify({policy: settings.policy ?? "advisory", source: "repo"})); break;
  case "scope-task": console.log(JSON.stringify({padding: "x".repeat(settings.scopePadding ?? 0), targets:[{path:"src/main.rs"}]})); break;
  case "execution-brief": {
    // The shape \`pixel execution-brief --json\` emits (execution_brief::from_scope_task).
    const quoted = "'" + args[1].replaceAll("'", "'\\\\''") + "'";
    const text = ["[PIXEL:EXECUTION_ROUTE]", "1. Run: pixel find-code " + quoted,
      "   If it returns no usable or relevant result, run exactly once: pixel find-code 'narrower behavior'",
      "2. Read: Read only a path returned by Pixel, in a maximum 40-line window around its line.",
      "3. If both Pixel calls do not converge, use: rg -m 5 -n -F -- 'task' . | sed -n '1,20p'",
      "4. Validate: After an edit, run the smallest relevant test for the changed behavior; read-only tasks need no test.",
      "[/PIXEL:EXECUTION_ROUTE]"].join("\\n");
    // Keys in the binary's order: serde_json sorts them, so the route text
    // comes before \`task\`.
    console.log(JSON.stringify({
      asks_about_code: settings.asksAboutCode ?? true,
      padding: "x".repeat(settings.scopePadding ?? 0),
      retrieval_route: { first_command: "pixel find-code " + quoted },
      ...(settings.legacyBrief ? {} : { retrieval_route_text: text }),
      task: args[1], version: 1,
      workstreams: [{ path: "src/main.rs", tier: "P0" }],
    }));
    break;
  }
  case "repo-state": console.log(JSON.stringify({branch:"fixture"})); break;
  case "find-code": console.log(JSON.stringify(settings.findAmbiguous ? {confidence:"ranked",matches:[{path:"src/a.rs",raw:"main",symbol_kind:"function"},{path:"src/b.rs",raw:"main",symbol_kind:"function"}]} : {padding: "x".repeat(settings.findPadding ?? 0), confidence:"resolved",matches:[{path:"src/found.rs",raw:"main",symbol_kind:"function"}]})); break;
  case "classify": {
    // No verdict configured is a cold engine: --if-warm exits 1, stdout empty.
    const verdict = settings.classify;
    if (!verdict) { console.error("not warm"); process.exit(1); }
    if (verdict.delay) await new Promise((done) => setTimeout(done, verdict.delay));
    appendFileSync(${JSON.stringify(trace)}, JSON.stringify(["classify-finished"]) + "\\n");
    console.log(verdict.raw ?? JSON.stringify({predicted: verdict.label, probs: {[verdict.label]: verdict.p, question: 1 - verdict.p}, snapshot: {model: "winnow:e4b", temperature: 0}, next_ops: verdict.ops}));
    process.exit(verdict.exit ?? 0);
  }
  case "what-changed": console.log(JSON.stringify({changed_files:1,risk:"LOW",symbols:[{change:"modified",name:readFileSync(${JSON.stringify(editedPath)}, "utf8"),path:"src/main.rs"}],suggested_tests:["main_tests"]})); break;
  default: console.log(JSON.stringify({ok:true,op:args[0]}));
}
`);
  chmodSync(binary, 0o755);
  const asset = readFileSync(new URL("../crates/pixel-install/assets/pi-pixel.ts", import.meta.url), "utf8");
  const source = asset.replace("__PIXEL_BIN__", JSON.stringify(binary))
    .replace('import { Type } from "@earendil-works/pi-ai";', 'const Type = new Proxy({}, {get: () => (...args) => args});');
  const extension = join(root, "pixel-guard.ts");
  writeFileSync(extension, source);
  const { default: activate } = await import(pathToFileURL(extension).href);
  let cwd = root;
  let storedEntries = [];
  const user = (text = "inspect the implementation") => ({
    cwd,
    sessionManager: { getSessionId: () => `fixture-${hostId}`, getLeafId: () => "leaf", getBranch: () => [{ type: "message", message: { role: "user", content: text } }, ...storedEntries] },
  });
  let hostId = 0;
  const host = async (mode, projectRoot = root) => {
    hostId += 1;
    storedEntries = [];
    cwd = projectRoot;
    restore("PIXEL_POLICY", mode);
    delete process.env.PIXEL_TARGETS_GUARD;
    // Every host (including fresh sub-projects in the policy loop) needs an
    // existing `src/` so the bash fence's canonical containment check has a
    // directory to canonicalize. The read tool stays lexical.
    mkdirSync(join(projectRoot, "src"), { recursive: true });
    const handlers = new Map();
    let tool;
    // A restored session can come back without the project's tools selected.
    let active = ["bash", "edit", "read"];
    const available = [...active, "pixel", "pixel_project"];
    const api = {
      registerTool: (registered) => { tool = registered; },
      on: (name, handler) => handlers.set(name, [...handlers.get(name) ?? [], handler]),
      getActiveTools: () => active,
      getAllTools: () => available.map((name) => ({ name })),
      setActiveTools: (names) => { active = names; },
      appendEntry: (customType, data) => storedEntries.push({ type: "custom", customType, data }),
    };
    activate(api);
    const emit = async (name, event, ctx = user()) => {
      let result;
      for (const handler of handlers.get(name) ?? []) {
        const next = await handler(event, ctx);
        if (next) result = next;
        if (next?.block) break;
      }
      return result;
    };
    await emit("session_start", { reason: "startup" });
    assert.ok(active.includes("pixel") && active.includes("pixel_project"), "session start activates both pixel tools");
    return { emit, tool, activateAgain: () => activate(api), boot: (prompt = "inspect the implementation") => emit("before_agent_start", { prompt }) };
  };
  const check = async (name, test) => {
    configure();
    await test();
    passed += 1;
    console.log(`ok ${passed} - ${name}`);
  };
  const native = (command, toolName = "bash") => ({ toolName, toolCallId: "shell", input: { command } });
  const edit = () => ({ toolName: "edit", toolCallId: "edit", input: { path: "src/main.rs" } });
  const read = (path = "src/main.rs", limit) => ({ toolName: "read", input: { path, ...(limit === undefined ? {} : { limit }) } });
  const pixelResult = (isError = false, toolName = "pixel") => ({ toolName, toolCallId: "pixel", input: {}, content: [{ type: "text", text: "{}" }], isError });
  const count = (op) => calls().filter(([name]) => name === op).length;

  await check("advisory default and invalid settings preserve every native input", async () => {
    for (const mode of [undefined, "advisory", "invalid"]) {
      const h = await host(mode);
      const boot = await h.boot();
      assert.match(boot.message.content, /Native tools remain available/);
      for (const event of [native("ls src"), native("cat src/main.rs"), read(), edit(), { toolName: "unfamiliar", input: {} }]) {
        const before = structuredClone(event);
        assert.equal(await h.emit("tool_call", event), undefined);
        assert.deepEqual(event, before);
      }
    }
  });

  await check("the task leads the budgeted brief, ahead of the route copy", async () => {
    const h = await host("advisory");
    const prompt = "Trace callers of the parser entry point " + "and its consumers ".repeat(80);
    const content = (await h.boot(prompt)).message.content;
    assert.ok(content.includes(JSON.stringify(prompt)), "the whole task survives the budget");
    assert.doesNotMatch(content, /retrieval_route_text|asks_about_code/);
    assert.equal(content.match(/\[PIXEL:EXECUTION_ROUTE\]/g).length, 1, "the route appears once");
  });
  await check("a prompt that asks nothing about code gets the repository state only", async () => {
    const h = await host("advisory");
    configure({ asksAboutCode: false });
    const boot = await h.boot("go to branch main and pull the latest changes");
    const content = boot.message.content;
    assert.match(content, /^PIXEL REPO STATE/);
    assert.match(content, /"branch":"fixture"/);
    assert.doesNotMatch(content, /PIXEL:EXECUTION_ROUTE|src\/main\.rs|TASK CONTEXT/);
    configure();
  });
  await check("a brief without the shared route text injects no guessed route", async () => {
    const h = await host("advisory");
    configure({ legacyBrief: true });
    const content = (await h.boot("Find the behavior that decides native repository reads")).message.content;
    assert.doesNotMatch(content, /PIXEL:EXECUTION_ROUTE/);
    assert.match(content, /Pixel execution route unavailable/);
    configure();
  });
  await check("prompt bootstrap injects an ordered populated route and the agent uses its bounded result", async () => {
    const h = await host("enforce");
    const prompt = "Find the behavior that decides native repository reads and explain its guard";
    const boot = await h.boot(prompt);
    const route = boot.message.content;
    // The same rendered route Claude, Codex and Devin receive.
    assert.match(route, /\[PIXEL:EXECUTION_ROUTE\]\n1\. Run: pixel find-code /);
    assert.ok(route.includes(`pixel find-code '${prompt}'`));
    assert.ok(route.indexOf("1. Run:") < route.indexOf("2. Read:"));
    assert.ok(route.indexOf("2. Read:") < route.indexOf("3. If both Pixel calls do not converge"));
    assert.match(route, /maximum 40-line window/);
    assert.match(route, /\[\/PIXEL:EXECUTION_ROUTE\]/);
    const briefCall = calls().findLast(([name]) => name === "execution-brief");
    assert.equal(briefCall[1], prompt, "the exact current prompt reaches execution-brief");
    assert.deepEqual(briefCall.slice(-2), ["--metrics", "off"], "bootstrap probes suppress metrics by design");

    const result = await h.tool.execute("find", { action: "find_code", goal: prompt }, null, null, user(prompt));
    assert.equal(result.content.length, 2);
    assert.match(result.content[0].text, /matches/);
    assert.equal(result.content[1].text, "🟩 pixel find-code ❀ 1.0ms\n  │\n  └───");
    await h.emit("tool_result", { toolName: "pixel", toolCallId: "pixel", input: {}, ...result });
    assert.equal(await h.emit("tool_call", read("src/found.rs", 40)), undefined, "the result path accepts a read bounded to the route's 40-line limit");
  });

  await check("configuration decides enforcement when the environment is silent", async () => {
    for (const [policy, blocked, guidance] of [["advisory", false, /Native tools remain available/], ["enforce", true, /Enforcement is enabled/], ["off", false, /Native tools remain available/]]) {
      configure({ policy });
      const project = mkdtempSync(join(root, `config-${policy}-`));
      const h = await host(undefined, project);
      assert.match((await h.boot()).message.content, guidance);
      const event = native("ls src");
      assert.equal(Boolean((await h.emit("tool_call", event))?.block), blocked, policy);
      assert.deepEqual(calls().find(([name]) => name === "config")?.slice(1), ["policy", "--json", "--metrics", "off"], policy);
    }
  });

  await check("enforcement blocks supported simple retrieval and gates edits explicitly", async () => {
    const h = await host("enforce");
    await h.boot();
    for (const event of [
      native("ls src"), native("ls -l src"), native("ls -la src"),
      native("rg error src"), native("rg -n error src"), native("rg -m 1 error src"),
      native("cat src/main.rs"), native("head src/main.rs"), native("tail src/main.rs"),
      native("git status"), native("git -C . log"), native("git --no-pager diff"),
      native("find src"), native("find src -name '*.rs'"),
      native("cp src/main.rs /tmp/pixel-leaf-dest"),
      read("src/unknown.rs", 100), edit(),
    ]) {
      const before = structuredClone(event);
      const decision = await h.emit("tool_call", event);
      assert.equal(decision?.block, true, before.input.command ?? before.toolName);
      assert.deepEqual(event, before);
    }
    const result = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(result.details.action, "find_code");
    assert.equal(await h.emit("tool_call", edit()), undefined);
    assert.equal(await h.emit("tool_call", read("src/found.rs", 200)), undefined);
    assert.equal((await h.emit("tool_call", read("src/found.rs", 201))).block, true);
    assert.equal((await h.emit("tool_call", read("src/found.rs"))).block, true);
  });

  await check("enforcement aligns bash reasons with the rust leaf table", async () => {
    const h = await host("enforce");
    await h.boot();
    const why = async (command) => {
      const event = native(command);
      const result = await h.emit("tool_call", event);
      return result ? JSON.parse(result.reason).redirect : undefined;
    };
    assert.equal(await why("cat src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("head src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("awk '{print}' src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("sed 's/a/b/' src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    // bounded sed read: should be ALLOWED (not blocked), so returns undefined
    assert.equal(await h.emit("tool_call", native("sed -n '1,20p' src/main.rs")), undefined);
    assert.equal(await h.emit("tool_call", native("rtk sed -n '1,20p' src/main.rs")), undefined);
    assert.equal(await why("cp src/main.rs /tmp/x"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("rg error src"), "repository search: use pixel search-content");
    assert.equal(await why("grep error src"), "repository search: use pixel search-content");
    // No path operand: the search runs over the cwd, the repository; the
    // pattern must not be read as a path that does not exist.
    assert.equal(await why("rg error"), "repository search: use pixel search-content");
    assert.equal(await why("grep -rn error"), "repository search: use pixel search-content");
    await h.emit("tool_result", pixelResult());
    assert.equal(await why("rg -m 5 -n -F -- error src"), undefined, "native search fallback proceeds after Pixel was used");
    assert.equal(await why("grep -m 5 error src"), undefined, "grep fallback proceeds after Pixel was used");
    assert.equal(await why("ls src"), "repository discovery: use pixel list-areas or find-code");
    assert.equal(await why("find src"), "repository discovery: use pixel find-code or list-areas");
    assert.equal(await why("git status"), "repository inspection: use pixel repo-state");
    assert.equal(await why("git diff"), "repository inspection: use pixel review-changes");
    assert.equal(await why("git log"), "repository inspection: use pixel commit-history");
  });

  await check("bash leaf operands honor the credential path regex", async () => {
    const h = await host("enforce");
    await h.boot();
    writeFileSync(join(root, ".env"), "K=v\n");
    for (const command of ["cat .env", "head .env", "rg -n needle .env", "cp .env /tmp/pixel-leaf-dest", "sed -n '1,20p' .env"]) {
      const event = native(command);
      const result = await h.emit("tool_call", event);
      assert.equal(result.block, true, command);
      assert.equal(JSON.parse(result.reason).redirect, "credential path", command);
      assert.equal(event.input.command, command);
    }
  });

  await check("a bounded sed read is exempt only for a readable repository file", async () => {
    const h = await host("enforce");
    await h.boot();
    const why = async (command) => {
      const event = native(command);
      const result = await h.emit("tool_call", event);
      return result ? JSON.parse(result.reason).redirect : undefined;
    };
    // A regular file inside the repository is what `is_bounded_sed_read`
    // grants the exemption to; a directory, a path under `.git`/`.pixel`, an
    // in-repo symlink to a credential and an out-of-window range are reads
    // the Rust guard still refuses.
    symlinkSync(join(root, ".env"), join(root, "notes.txt"));
    mkdirSync(join(root, ".git"), { recursive: true });
    writeFileSync(join(root, ".git/config"), "[core]\n");
    writeFileSync(join(root, ".pixel/notes.txt"), "notes\n");
    for (const command of [
      "sed -n '1,20p' src", "sed -n '1,20p' notes.txt", "rtk sed -n '1,20p' notes.txt",
      "sed -n '1,20p' .pixel/notes.txt", "sed -n '1,20p' .git/config", "sed -n '1,201p' src/main.rs",
    ]) {
      assert.equal(await why(command), "repository read: use pixel search-content or pixel pack-context <uid>", command);
    }
    // Missing paths, paths outside the repository and in-place edits keep
    // their native handling, as they do in the guard.
    for (const command of ["sed -n '1,20p' missing.rs", "sed -n '1,20p' /tmp/external.txt", "sed -i 's/a/b/' src/main.rs"]) {
      assert.equal(await h.emit("tool_call", native(command)), undefined, command);
    }
  });

  await check("enforcement preserves compositions, quoted punctuation and unknown capabilities", async () => {
    const h = await host("enforce");
    await h.boot();
    for (const toolName of ["bash", "run_command"]) {
      for (const command of [
        "cargo test | tail -20", "cargo test | rg error", "pixel repo-state --json | jq .branch",
        "cargo test > output.log", "cargo test && rg error output.log",
        "echo 'cat src/main.rs'", "printf 'a|b;$(literal)'", "cat src/a\\ b.rs",
        "git fetch origin", "pixel find-code main", "python3 inspect.py", "lsfoo src",
        "mv src/main.rs /tmp/pixel-move", "cat /tmp/external.txt",
        "lsof", "lsblk", "catapult", "ls /tmp", "grep needle /tmp/external.txt",
        "cd src && ls src", "command ls src", "builtin echo src",
        "find /tmp -name 'x'", "rg -m 1 needle /etc/hosts",
      ]) {
        const event = native(command, toolName);
        assert.equal(await h.emit("tool_call", event), undefined, command);
        assert.equal(event.input.command, command);
      }
    }
    assert.equal(await h.emit("tool_call", { toolName: "unfamiliar", input: {} }), undefined);
    for (const toolName of ["grep", "find", "ls", "glob", "list_dir", "grep_search", "file_search"]) {
      assert.equal(await h.emit("tool_call", { toolName, input: { path: "/tmp/external" } }), undefined);
    }
  });

  await check("off and the legacy opt-out bypass policy classification and audit", async () => {
    for (const optout of [undefined, "0", "false", "OFF"]) {
      const h = await host(optout === undefined ? "off" : "enforce");
      restore("PIXEL_TARGETS_GUARD", optout);
      await h.boot();
      const auditPath = join(root, ".pixel/pi-policy.jsonl");
      const before = readFileSync(auditPath, "utf8");
      for (const event of [native("ls src"), read(), edit()]) assert.equal(await h.emit("tool_call", event), undefined);
      assert.equal(readFileSync(auditPath, "utf8"), before);
    }
  });

  await check("post-edit context follows execution and preserves full native results", async () => {
    const h = await host();
    const before = count("what-changed");
    await h.emit("tool_call", edit());
    assert.equal(count("what-changed"), before);
    writeFileSync(editedPath, "after_edit");
    const event = { ...edit(), content: [{ type: "text", text: "Edit applied: 2 lines" }, { type: "image", data: "fixture", mimeType: "image/png" }], details: { diff: "+ updated", firstChangedLine: 8 }, isError: false };
    const result = await h.emit("tool_result", event);
    assert.deepEqual(result.content.slice(0, 2), event.content);
    assert.match(result.content[2].text, /modified  after_edit  src\/main.rs/);
    assert.deepEqual(result.details, event.details);
    assert.equal(result.isError, false);
    assert.equal(count("what-changed"), before + 1);
  });

  await check("failed edits retain their diagnostics and do not collect impact", async () => {
    const h = await host();
    const before = count("what-changed");
    const event = { ...edit(), content: [{ type: "text", text: "Exact match not found" }], details: { path: "src/main.rs" }, isError: true };
    const original = structuredClone(event);
    assert.equal(await h.emit("tool_result", event), undefined);
    assert.deepEqual(event, original);
    assert.equal(count("what-changed"), before);
  });

  await check("unavailable post-edit context preserves successful native output", async () => {
    const h = await host("enforce");
    await h.boot();
    configure({ fail: ["what-changed"] });
    const event = { ...edit(), content: [{ type: "text", text: "Edit applied" }], details: { diff: "+ changed" }, isError: false };
    assert.equal(await h.emit("tool_result", event), undefined);
    assert.equal(event.content[0].text, "Edit applied");
    assert.equal(await h.emit("tool_call", native("ls src")), undefined);
  });

  await check("bootstrap and structured-tool failures fail open and recover", async () => {
    for (const op of ["status", "execution-brief", "repo-state"]) {
      const h = await host("enforce");
      configure({ fail: [op] });
      const boot = await h.boot();
      assert.match(boot.message.content, /PIXEL UNAVAILABLE/);
      assert.equal(await h.emit("tool_call", edit()), undefined);
      assert.equal(await h.emit("tool_call", native("ls src")), undefined);
      configure();
      await h.boot();
      assert.equal((await h.emit("tool_call", edit())).block, true);
      configure({ fail: ["find-code"] });
      const result = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
      assert.match(result.details.error, /fixture Pixel unavailable/);
      assert.equal(await h.emit("tool_call", edit()), undefined);
      configure();
    }
  });

  await check("missing route capability fails open without blocking native retrieval", async () => {
    const h = await host("enforce");
    configure({ missing: ["list-areas"] });
    await h.boot();
    assert.equal(await h.emit("tool_call", native("ls src")), undefined);
    assert.equal(await h.emit("tool_call", edit()), undefined);
  });

  await check("complete bootstrap and tool evidence resolve paths before truncation", async () => {
    const h = await host("enforce");
    configure({ scopePadding: 3000, findPadding: 17000 });
    const boot = await h.boot();
    assert.match(boot.message.content, /truncated/);
    assert.equal((await h.emit("tool_call", read("src/main.rs", 200))).block, true, "bootstrap paths alone do not unlock reads");
    assert.equal(await h.emit("tool_result", pixelResult()), undefined);
    assert.equal(await h.emit("tool_call", read("src/main.rs", 200)), undefined);
    const found = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(found.details.truncated, true);
    assert.equal(await h.emit("tool_call", read("src/found.rs", 200)), undefined);
  });

  await check("bounded reads unlock only after a successful pixel result", async () => {
    const h = await host("enforce");
    await h.boot();
    assert.equal((await h.emit("tool_call", read("src/main.rs", 100))).block, true, "blocked before any pixel call");
    await h.emit("tool_result", pixelResult(true));
    assert.equal((await h.emit("tool_call", read("src/main.rs", 100))).block, true, "a failed pixel result does not unlock");
    await h.emit("tool_result", pixelResult(false, "pixel_project"));
    assert.equal(await h.emit("tool_call", read("src/main.rs", 100)), undefined, "allowed after a successful result");
    assert.equal((await h.emit("tool_call", read("src/main.rs", 201))).block, true, "the read limit still applies");
  });

  await check("an unauthorized pixel_project commit is an error and does not count as a call", async () => {
    const h = await host("enforce");
    await h.boot();
    const denied = await h.tool.execute("denied", { action: "commit", files: ["src/main.rs"], message: "x", request_id: "r" }, null, null, user("do not commit"));
    assert.match(denied.content[0].text, /authorization.*absent/);
    assert.equal(denied.isError, true, "a denied commit is an error result");
    const relayed = await h.emit("tool_result", { toolName: "pixel_project", toolCallId: "denied", input: {}, ...denied, isError: false });
    assert.equal(relayed.isError, true, "the error-shaped payload reaches the tool_result session message");
    assert.equal((await h.emit("tool_call", read("src/main.rs", 100))).block, true, "a denied commit does not unlock reads");
  });

  await check("a successful tool result carries the metrics box as its own content item", async () => {
    const h = await host("enforce");
    await h.boot();
    const result = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(result.content.length, 2);
    assert.match(result.content[0].text, /^\{/);
    assert.equal(result.content[1].text, "\u{1F7E9} pixel find-code \u2740 1.0ms\n  \u2502\n  \u2514\u2500\u2500\u2500");
    assert.doesNotMatch(result.content[0].text + result.content[1].text, /warning: diagnostic/);
    const quiet = calls().filter(([name]) => ["status", "scope-task", "repo-state"].includes(name));
    assert.ok(quiet.length > 0 && quiet.every((args) => args.slice(-2).join(" ") === "--metrics off"), "probes stay silent");
    const shown = calls().filter(([name]) => name === "find-code");
    assert.ok(shown.length > 0 && shown.every((args) => !args.includes("off")), "tool runs keep metrics");
  });

  await check("blocked reads say what was wrong and a Pixel call alone unlocks in-repo reads", async () => {
    const h = await host("enforce");
    await h.boot();
    const why = async (event) => JSON.parse((await h.emit("tool_call", event)).reason).redirect;
    const tail = ". Call pixel first, then read with a limit of at most 200 lines";
    assert.equal(await why(read("src/other.rs", 100)), "Read blocked: path not resolved by pixel yet" + tail);
    assert.equal(await why(read("src/other.rs")), "Read blocked: no limit given" + tail);
    assert.equal(await why(read("src/other.rs", 300)), "Read blocked: limit 300 exceeds 200" + tail);
    await h.emit("tool_result", pixelResult());
    assert.equal(await h.emit("tool_call", read("src/never-resolved.rs", 200)), undefined, "global pixel result unlocks any in-repo path");
    assert.equal(await why(read("src/other.rs", 300)), "Read blocked: limit 300 exceeds 200" + tail);
    assert.equal(await why(read(".env", 10)), "Read blocked: credential path" + tail);
    assert.equal(await why(read("@.env", 10)), "Read blocked: credential path" + tail);
    assert.equal(await why(read("keys/id_rsa", 10)), "Read blocked: credential path" + tail);
    assert.equal(await h.emit("tool_call", read("/etc/hosts", 10)), undefined, "outside the repository stays native");
  });

  await check("short prompts and session changes never retain stale edit or path state", async () => {
    const h = await host("enforce");
    assert.equal(await h.emit("before_agent_start", { prompt: "fix" }), undefined);
    assert.equal(await h.emit("tool_call", edit()), undefined);
    await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(await h.emit("tool_call", read("src/found.rs", 100)), undefined);
    await h.emit("session_start", { reason: "new" });
    assert.equal(await h.emit("tool_call", edit()), undefined);
    await h.boot();
    assert.equal((await h.emit("tool_call", edit())).block, true);
    assert.equal((await h.emit("tool_call", read("src/found.rs", 100))).block, true);
  });

  await check("structured fetch stays isolated and commit/push authorization survives off", async () => {
    const h = await host("off");
    const fetchBefore = count("fetch");
    const pushBefore = count("commit-and-push");
    const params = { action: "commit_and_push", files: ["src/main.rs"], message: "test", request_id: "fixture" };
    await h.tool.execute("fetch", { action: "fetch", remote: "origin" }, null, null, user("fetch only"));
    assert.equal(count("fetch"), fetchBefore + 1);
    for (const text of ["fetch only", "do not commit and push", "don't commit yet", "commit messages look wrong"]) {
      const result = await h.tool.execute("denied", params, null, null, user(text));
      assert.match(result.content[0].text, /authorization.*absent/);
    }
    assert.equal(count("commit-and-push"), pushBefore);
    await h.tool.execute("allowed", params, null, null, user("commit and push this change"));
    assert.equal(count("commit-and-push"), pushBefore + 1);
  });
  await check("pixel tool results resolve read targets and bare symbols reach pack-context", async () => {
    const h = await host("enforce");
    await h.boot();
    await h.emit("tool_result", {
      toolName: "pixel", isError: false,
      content: [{ type: "text", text: JSON.stringify({ targets: [{ path: "src/other.rs" }] }) }],
    });
    assert.equal(await h.emit("tool_call", read("src/other.rs", 120)), undefined);
    assert.equal((await h.emit("tool_call", read("src/other.rs", 201))).block, true);
    const before = count("pack-context");
    await h.tool.execute("pack", { action: "pack_context", symbol: "main" }, null, null, user());
    const packCalls = calls().filter(([name]) => name === "pack-context");
    assert.equal(packCalls.length, before + 1);
    assert.ok(packCalls.at(-1).includes("src/found.rs#main#function"), JSON.stringify(packCalls.at(-1)));
  });
  await check("ambiguous pack_context target returns find_code candidate uids", async () => {
    configure({ findAmbiguous: true });
    const h = await host("enforce");
    const result = await h.tool.execute("pack", { action: "pack_context", symbol: "main" }, null, null, user());
    const details = JSON.parse(result.content[0].text);
    assert.match(details.next_action, /find_code/);
    assert.match(details.next_action, /src\/a\.rs#main#function/);
    const packCalls = calls().filter(([name]) => name === "pack-context");
    assert.ok(packCalls.at(-1).includes('"main"') || packCalls.at(-1).includes("main"), JSON.stringify(packCalls.at(-1)));
  });

  const bugfix = { label: "bugfix", p: 0.8765, ops: ["pixel plan-rollback \"<problem>\"", "pixel impact \"<symbol>\""] };
  const intentLine = /^Intent \(classifier verdict, not fact\)/m;

  await check("a warm task-intent verdict adds one classifier line to the bootstrap", async () => {
    const h = await host();
    configure({ classify: bugfix });
    const content = (await h.boot()).message.content;
    assert.match(content, /^PIXEL TASK CONTEXT/);
    assert.equal(content.split("\n").filter((line) => intentLine.test(line)).length, 1);
    assert.ok(content.endsWith('\n\nIntent (classifier verdict, not fact): bugfix p=0.88 (winnow:e4b) → start with: pixel plan-rollback "<problem>", pixel impact "<symbol>"'), content);
    const call = calls().findLast(([name]) => name === "classify");
    assert.deepEqual(call, ["classify", "--task-intent", "--if-warm", "--json", "--metrics", "off", "--", "inspect the implementation"]);
  });

  await check("no intent line when the classifier exits nonzero, is unsure, unparsable or malformed", async () => {
    for (const settings of [
      {},
      { classify: { ...bugfix, exit: 1 } },
      { classify: { ...bugfix, p: 0.49 } },
      { classify: { ...bugfix, raw: "not json" } },
      { classify: { ...bugfix, ops: [] } },
      { classify: { ...bugfix, ops: [null] } },
      { classify: { ...bugfix, ops: [""] } },
      { classify: { ...bugfix, ops: ["   "] } },
      { classify: { ...bugfix, ops: [bugfix.ops[0], null] } },
      { classify: bugfix, missing: ["classify"] },
    ]) {
      const h = await host();
      configure(settings);
      const content = (await h.boot()).message.content;
      assert.match(content, /^PIXEL TASK CONTEXT/, JSON.stringify(settings));
      assert.doesNotMatch(content, /Intent|classifier/, JSON.stringify(settings));
    }
  });

  await check("a slow classifier is killed at the deadline and the bootstrap still arrives", async () => {
    const h = await host();
    configure({ classify: { ...bugfix, delay: 700 } });
    const before = count("classify-finished");
    const started = Date.now();
    const content = (await h.boot()).message.content;
    const elapsed = Date.now() - started;
    assert.match(content, /^PIXEL TASK CONTEXT/);
    assert.doesNotMatch(content, /Intent|classifier/);
    // The fixture's own termination evidence for the deadline: its classifier
    // needs 700 ms, past the 500 ms kill, so a bootstrap that returned with no
    // completion marker cannot have waited for it.
    assert.equal(count("classify-finished"), before, "the bootstrap returned before the classifier answered");
    // The marker is still absent once the full delay has elapsed: the child
    // was killed rather than left running.
    await new Promise((done) => setTimeout(done, 1200));
    assert.equal(count("classify-finished"), before, "the timed-out classifier was killed before it answered");
    // A separate, loose hang bound for the whole bootstrap: slow health checks
    // or context collection must not read as a missed deadline.
    assert.ok(elapsed < 5000, `bootstrap waited ${elapsed} ms`);
  });

  await check("short prompts never spawn the classifier", async () => {
    const h = await host();
    configure({ classify: bugfix });
    const before = count("classify");
    assert.equal(await h.emit("before_agent_start", { prompt: "fix" }), undefined);
    assert.equal(count("classify"), before);
  });
  await check("task gates apply even when retrieval policy is off", async () => {
    const h = await host("off");
    configure({ task: { "pre-tool-use": { decision: "deny", reason: "Prepare task before editing" } } });
    assert.deepEqual(await h.emit("tool_call", edit()), { block: true, reason: "Prepare task before editing" });
    configure({ task: { "pre-tool-use": { decision: "allow" } } });
    assert.equal(await h.emit("tool_call", edit()), undefined);
  });
  await check("missing task state closes edits while recovery stays available", async () => {
    const h = await host("off");
    configure({ fail: ["run-hook"] });
    for (const event of [edit(), native("python mutate.py"), native("cat x > y")]) {
      assert.equal((await h.emit("tool_call", event)).block, true);
    }
    for (const event of [read(), native("rtk proxy pixel task status"), native("git diff")]) {
      assert.equal(await h.emit("tool_call", event), undefined);
    }
    const gate = await h.emit("agent_before_settle", { outcome: "completed", context: { canContinue: true } });
    assert.equal(gate.continue, false);
    assert.match(gate.entries[0].content, /task state is unavailable/);
  });
  await check("read sequences stay available when task state is unavailable", async () => {
    const h = await host("off");
    configure({ fail: ["run-hook"] });
    for (const command of [
      "cd src && cat main.rs", "ls src 2>/dev/null; cat src/main.rs", "rg needle || cat src/main.rs",
      "nl -ba src/main.rs | head -n 40", "echo start; cat src/main.rs",
    ]) assert.equal(await h.emit("tool_call", native(command)), undefined, command);
    for (const command of [
      "cat src/main.rs 2>/tmp/err", "rg needle; rm src/main.rs", "cd src && rm main.rs", "rg needle; pixel task prepare task-1",
      "cd src && sed -i 's/a/b/' main.rs",
    ]) assert.equal((await h.emit("tool_call", native(command))).block, true, command);
  });
  await check("known discovery aliases remain available when task state is unavailable", async () => {
    const h = await host("off");
    configure({ fail: ["run-hook"] });
    for (const toolName of ["ls", "find", "list_dir", "grep_search", "file_search", "view_file"]) {
      assert.equal(await h.emit("tool_call", { toolName, toolCallId: `read-${toolName}`, input: { path: "src/lib.rs" } }), undefined, toolName);
    }
  });
  await check("write-capable read flags, config setters and unknown tools require task state", async () => {
    const h = await host("off");
    configure({ fail: ["run-hook"] });
    for (const command of [
      "git diff --output=src/a.rs", "git diff --output src/a.rs", "git show --ext-diff",
      "rg --pre /runner/mutator pattern", "rg --pre=/runner/mutator pattern", "rg --hostname-bin /runner/mutator pattern",
      "pixel config policy off", "pixel config edit --repo", "pixel config setup",
      "pixel task evaluate --suite external.json", "pixel task reset session",
      "pixel task-state evaluate --suite external.json", "pixel task-state reset session",
    ]) assert.equal((await h.emit("tool_call", native(command))).block, true, command);
    for (const command of [
      "git diff --name-only HEAD", "git status --porcelain=v1", "git diff -- --output=notes",
      "rg -n -F needle src", "rg --glob='*.rs' needle", "rg -- --pre",
      "pixel config", "pixel config policy", "pixel config metrics", "pixel task prepare task-1",
      "pixel task verify task-1", "pixel task recover task-1", "pixel task contract task-1 --file contract.json",
      "pixel task-state status task-1 --json", "pixel task-state contract task-1 --definition '{}'",
    ]) assert.equal(await h.emit("tool_call", native(command)), undefined, command);
    for (const toolName of ["mcp__custom__edit", "customTool", ""]) {
      assert.equal((await h.emit("tool_call", { toolName, toolCallId: "unknown", input: {} })).block, true, toolName);
    }
    assert.equal((await h.emit("tool_call", { toolName: "pixel", toolCallId: "unknown-action", input: { action: "new_mutator" } })).block, true);
  });
  await check("proven read pipelines and literal inline contracts remain available during recovery", async () => {
    const h = await host("off");
    configure({ fail: ["run-hook"] });
    const definition = JSON.stringify({ checks: [{ argv: ["/bin/sh", "-c", "test \"$(cat source.txt)\" = original"] }] });
    for (const command of ["rg needle src | sort | uniq", "git diff --name-only | sort -u", "rg 'x|y' src | uniq -c", "cat source.txt | uniq -- -", "rg needle || cat source.txt", "rg needle; cat source.txt", `pixel task contract task-1 --definition '${definition}' --json`]) {
      assert.equal(await h.emit("tool_call", native(command)), undefined, command);
    }
    for (const command of ["rg needle | sort -o source.txt", "rg needle | sort --output=source.txt", "rg needle | uniq - source.txt", "rg needle | uniq -- - source.txt", "rg needle | tee source.txt", "rg needle | pixel task prepare task-1", "rg needle | pixel task-state prepare task-1", "rg needle; pixel task prepare task-1", "rg needle || rm source.txt", "cat source.txt && sed -i s/a/b/ source.txt", "rg needle | cat > source.txt", "rg $(touch source.txt) | sort", "rg \"$(touch source.txt)\" | sort", "pixel task contract task-1 --definition $(cat secret)", "pixel task contract task-1 --definition \"$(cat secret)\""]) {
      assert.equal((await h.emit("tool_call", native(command))).block, true, command);
    }
  });
  await check("settlement uses core continuation budget and never restarts cancellation or errors", async () => {
    const h = await host("off");
    configure({ task: { stop: { decision: "continue", reason: "Run required check" } } });
    const gate = await h.emit("agent_before_settle", { outcome: "completed", context: { canContinue: true } });
    assert.equal(gate.continue, true);
    assert.deepEqual(gate.entries, [{ type: "custom_message", customType: "pixel-task-gate", content: "Run required check", display: true }]);
    assert.equal((await h.emit("agent_before_settle", { outcome: "completed", context: { canContinue: false } })).continue, false);
    for (const outcome of ["aborted", "error"]) assert.equal(await h.emit("agent_before_settle", { outcome, context: { canContinue: true } }), undefined);
    configure({ task: { stop: { decision: "deny", reason: "Continuation budget exhausted" } } });
    assert.equal((await h.emit("agent_before_settle", { outcome: "completed", context: { canContinue: true } })).continue, false);
  });
  await check("task subprocess is cancelled without forcing another provider request", async () => {
    const h = await host("off");
    configure({ taskDelay: 3000, task: { stop: { decision: "continue" } } });
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), 50);
    const before = Date.now();
    const result = await h.emit("agent_before_settle", { outcome: "completed", context: { canContinue: true } }, { ...user(), signal: abort.signal });
    clearTimeout(timer);
    assert.equal(result, undefined);
    assert.ok(Date.now() - before < 2000);
  });
  await check("model requests retain real identities and private bang commands never enter telemetry", async () => {
    const h = await host("off");
    await h.emit("message_end", { message: { role: "assistant", content: [{ type: "toolCall", id: "real-call", name: "edit", arguments: { secret: "private" } }], stopReason: "toolUse", responseId: "response-7", usage: { input: 100, output: 10, cacheRead: 50, cacheWrite: 2, cost: { total: 1 }, secret: "private" } } });
    await h.emit("user_bash", { command: "private-bang-token", excludeFromContext: true });
    const events = readFileSync(taskTrace, "utf8").trim().split("\n").map(JSON.parse);
    const message = events.findLast((entry) => entry.event === "model-response");
    assert.deepEqual(message.payload.request_ids, ["real-call"]);
    assert.deepEqual(message.payload.request_tools, { "real-call": "edit" });
    assert.deepEqual(message.payload.usage, { input: 100, output: 10, cache_read: 50, cache_write: 2 });
    assert.equal(message.payload.response_id, "response-7");
    assert.equal(message.payload.coverage_complete, true);
    const bang = events.at(-1);
    assert.equal(bang.event, "user-bash");
    assert.ok(!JSON.stringify(bang).includes("private-bang-token"));
    assert.ok(!JSON.stringify(message).includes("private"));
  });
  await check("duplicate extension ownership emits one gate and session trees retain branch identity", async () => {
    const h = await host("off");
    h.activateAgain();
    const before = count("run-hook");
    await h.emit("tool_call", edit());
    assert.equal(count("run-hook"), before + 1);
    await h.emit("session_tree", { newLeafId: "branch-leaf" });
    await h.emit("tool_call", edit());
    const events = readFileSync(taskTrace, "utf8").trim().split("\n").map(JSON.parse);
    assert.equal(events.at(-1).payload.branch_id, "branch-leaf");
    assert.equal(events.at(-1).payload.toolCallId, "edit");
  });
  await check("branch-local task bindings survive reload and unbound navigation is explicit", async () => {
    const h = await host("off");
    configure({ task: { "prompt-submit": { decision: "observe", task_id: "task-real", attempt_id: "attempt-real" } } });
    await h.emit("before_agent_start", { prompt: "fix" });
    assert.equal(storedEntries.at(-1).customType, "pixel-task-binding");
    configure();
    await h.emit("session_start", { reason: "reload" });
    await h.emit("tool_call", edit());
    let last = JSON.parse(readFileSync(taskTrace, "utf8").trim().split("\n").at(-1));
    assert.equal(last.payload.task_id, "task-real");
    assert.equal(last.payload.attempt_id, "attempt-real");
    assert.equal(last.payload.branch_unbound, false);
    const origin = last.payload.session_id;
    hostId += 1;
    await h.emit("session_start", { reason: "fork" });
    await h.emit("tool_call", edit());
    last = JSON.parse(readFileSync(taskTrace, "utf8").trim().split("\n").at(-1));
    assert.equal(last.payload.binding_session_id, origin);
    assert.notEqual(last.payload.session_id, origin);
    assert.equal(last.payload.task_id, "task-real");
    storedEntries = [];
    await h.emit("session_tree", { newLeafId: "unknown-branch" });
    await h.emit("tool_call", edit());
    last = JSON.parse(readFileSync(taskTrace, "utf8").trim().split("\n").at(-1));
    assert.equal(last.payload.task_id, undefined);
    assert.equal(last.payload.branch_unbound, true);
  });
  console.log(`Pi extension: ${passed} event-handler contract groups passed`);
} finally {
  restore("PIXEL_POLICY", originalEnv[0]);
  restore("PIXEL_TARGETS_GUARD", originalEnv[1]);
  rmSync(root, { recursive: true, force: true });
}
