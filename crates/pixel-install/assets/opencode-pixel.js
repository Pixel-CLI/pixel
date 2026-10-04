// pixel guard for OpenCode — managed by `pixel install`.
// __MANAGED_BEGIN__
// __MANAGED_END__
//
// ## Why this file exists at all
//
// `pixel install` also writes the retrieval contract into OpenCode's global
// `AGENTS.md`. That is advice, and advice loses: a model reads "use pixel
// search-content" and then runs `rtk git log` anyway. OpenCode is the only
// supported host whose plugin hook can *stop* a call — `tool.execute.before`
// may throw — so it is the one host where the substitutions and refusals pixel
// already computes were reachable in principle and unreachable in practice.
//
// The policy is deliberately not reimplemented here. Every decision is
// delegated to `pixel hook guard --provider opencode`, the same code path
// Claude Code, Codex, Devin, zcode and Antigravity use, so OpenCode cannot
// drift from what the other hosts enforce.
//
// ## The host contract
//
// Measured against opencode 1.18.34 with a probe plugin rather than assumed,
// because each of these was silently wrong in a first attempt:
//
//   - `input.tool` is the lowercase tool id ("bash", "read", "grep"). The
//     guard's `opencode_tool_input` normalises it to the spelling its policy
//     arms match on, so this file must NOT title-case it: doing so defeated
//     that normaliser and every call came back "allow".
//   - The read tool's path argument is `filePath`; likewise passed through.
//   - A rewrite is applied by mutating a key *inside* `output.args`.
//     Replacing the `args` object is silently dropped by the host, so the
//     rewrite computes correctly and then never lands.
//   - A deny is enforced by throwing; the message is shown to the model.
//
// ## Failure policy
//
// An unavailable pixel must never brick the editor. A missing binary, a
// timeout, malformed output, an unrecognised payload: all of them resolve to
// "no opinion" and the call proceeds exactly as it would without this plugin.

import { spawn, spawnSync } from "node:child_process";

/** Absolute path to the pixel that installed this file. */
const PIXEL_BIN = __PIXEL_BIN__;

/**
 * Tool ids this guard forwards.
 *
 * A set of ids, deliberately not a rename map — see the note above on
 * `tool_name`. A tool that is not listed is the host's business.
 */
const GUARDED_TOOLS = new Set([
  "bash",
  "read",
  "write",
  "edit",
  "patch",
  "grep",
  "glob",
  "list",
  "webfetch",
]);

/** Never spend longer than this on the guard; fail open past it. */
const TIMEOUT_MS = 5_000;

/**
 * Ask pixel about one tool call.
 *
 * Resolves to the guard's response envelope, or null when it has nothing to
 * say: allow, no active manifest, no index, or any failure. Every one of those
 * is an ordinary outcome rather than an error.
 */
function askPixel(tool, args, directory) {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (value) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };

    let child;
    try {
      child = spawn(PIXEL_BIN, ["hook", "guard", directory, "--provider", "opencode"], {
        cwd: directory,
        stdio: ["pipe", "pipe", "ignore"],
        env: { ...process.env, PIXEL_METRICS: "off" },
      });
    } catch {
      return finish(null);
    }

    let out = "";
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      finish(null);
    }, TIMEOUT_MS);

    child.stdout?.on("data", (chunk) => (out += chunk));
    child.on("error", () => {
      clearTimeout(timer);
      finish(null);
    });
    child.on("close", () => {
      clearTimeout(timer);
      if (!out.trim()) return finish(null);
      try {
        finish(JSON.parse(out));
      } catch {
        finish(null);
      }
    });

    try {
      child.stdin.write(
        `${JSON.stringify({
          hook_event_name: "PreToolUse",
          tool_name: tool,
          tool_input: args ?? {},
          // Load-bearing: the guard resolves the repository from this field
          // and falls back to *its own* cwd when it is absent — the opencode
          // process's directory rather than the session's. It then reads a
          // different repository's manifest and answers "allow" to
          // everything.
          cwd: directory,
        })}\n`,
      );
      child.stdin.end();
    } catch {
      clearTimeout(timer);
      finish(null);
    }
  });
}

/**
 * The rewritten arguments, if the response carries a rewrite.
 *
 * `hookSpecificOutput.updatedInput` is the Claude/Codex shape. A response
 * holding only a decision is a refusal and has nothing to rewrite.
 */
function rewrittenArgs(response) {
  const updated = response?.hookSpecificOutput?.updatedInput;
  return updated && typeof updated === "object" ? updated : null;
}

/** The refusal reason, whichever envelope shape carried it. */
function denyReason(response) {
  const specific = response?.hookSpecificOutput ?? {};
  return (
    specific.permissionDecisionReason ??
    response?.reason ??
    "blocked by pixel policy"
  );
}

export const PixelGuardPlugin = async ({ client, directory }) => {
  try {
    await client?.app?.log?.({
      body: {
        service: "pixel-guard",
        level: "debug",
        message: "pixel guard loaded",
      },
    });
  } catch {
    // Logging must never break the plugin.
  }

  return {
    "tool.execute.before": async (input, output) => {
      if (!GUARDED_TOOLS.has(input?.tool)) return;

      const response = await askPixel(input.tool, output?.args, directory);
      if (!response) return;

      // Substitute first. pixel rewrites an equivalent command rather than
      // failing the call, and the agent is never told it happened.
      const updated = rewrittenArgs(response);
      if (updated) {
        for (const [key, value] of Object.entries(updated)) {
          output.args[key] = value;
        }
        return;
      }

      const decision =
        response?.hookSpecificOutput?.permissionDecision ?? response?.decision;
      if (decision === "deny" || response?.decision === "block") {
        throw new Error(denyReason(response));
      }
    },
  };
};

// Keep a V2-native route tool alongside the V1 guard. OpenCode V2 does not
// load V1 plugin functions, while V1.18.29+ accepts the `server` property on
// this dual-entrypoint object. The route tool itself is deliberately
// fail-open: a missing binary or stale index is returned as a native-tool
// fallback, never thrown from a hook.
function metricsBox(stderr) {
  const lines = String(stderr ?? "").split("\n");
  const start = lines.findIndex((line) => line.includes("🟩") && line.toLowerCase().includes("pixel"));
  if (start < 0) return "";
  const rest = lines.slice(start);
  const end = rest.findIndex((line) => line.trimStart().startsWith("└"));
  return rest.slice(0, end < 0 ? rest.length : end + 1).join("\n").trim();
}

function executionBrief(task, directory, run = spawnSync, pixelBin = process.env.PIXEL_BIN || PIXEL_BIN) {
  const result = run(pixelBin, ["execution-brief", task, "--json"], {
    cwd: directory,
    encoding: "utf8",
    timeout: TIMEOUT_MS,
    maxBuffer: 2_000_000,
    env: { ...process.env },
  });
  if (result.error || result.status !== 0) {
    throw new Error(result.error?.message ?? result.stderr?.trim() ?? `pixel exited ${result.status}`);
  }
  return { route: JSON.parse(result.stdout), metrics: metricsBox(result.stderr) };
}

const PixelRoutePluginV2 = {
  id: "pixel",

  async setup(ctx, spawnProcess = spawnSync) {
    const registrations = [];
    const runBrief = (task, directory) => executionBrief(task, directory, spawnProcess);
    try {
      const registration = await ctx.tool.transform((editor) => {
        editor.add({
            name: "pixel",
            description:
              "Get Pixel's deterministic task route first: one populated starting command, " +
              "ordered bounded reads, fallback, and minimal validation. Returns route JSON " +
              "plus the exact metrics line for this invocation.",
            input: {
              type: "object",
              properties: {
                action: { type: "string", enum: ["execution_brief"] },
                task: {
                  type: "string",
                  description: "the task to turn into an ordered retrieval route",
                },
              },
              required: ["action", "task"],
              additionalProperties: false,
            },
            async execute(input, context) {
              const task = typeof input?.task === "string" ? input.task.trim() : "";
              if (input?.action !== "execution_brief" || !task) {
                return {
                  content: [{
                    type: "text",
                    text: JSON.stringify({
                      error: "execution_brief requires a task; continue with native tools if no route is available",
                    }),
                  }],
                };
              }
              try {
                const directory = context?.directory ?? ctx.location?.directory ?? process.cwd();
                const { route, metrics } = runBrief(task, directory);
                const content = [{
                  type: "text",
                  text: JSON.stringify({ action: "execution_brief", task, route }),
                }];
                if (metrics) content.push({ type: "text", text: metrics });
                return { content };
              } catch (error) {
                return {
                  content: [{
                    type: "text",
                    text: JSON.stringify({
                      action: "execution_brief",
                      task,
                      error: String(error),
                      fallback: "Pixel route unavailable; continue with native tools.",
                    }),
                  }],
                };
              }
            },
        });
      });
      if (registration?.dispose) registrations.push(registration);
    } catch {
      // A tool API mismatch must not make OpenCode startup fail.
    }
    return () => {
      for (const registration of registrations) {
        try { registration.dispose(); } catch {}
      }
    };
  },
};

// V1.18.29+ reads `server`; V2 reads `id` + `setup`. The implementations are
// intentionally separate because OpenCode's V2 API is not a V1 translation.
export default { ...PixelRoutePluginV2, server: PixelGuardPlugin };
