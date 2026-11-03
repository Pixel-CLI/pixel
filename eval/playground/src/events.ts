// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Turns the `codex exec --json` event stream into what a pane shows: one line
// per tool call, the final answer, token usage and the search/read split.
// Pure, shared by the browser and the tests.

export type ToolKind = "search" | "read" | "other";
export type Tool = { id: string; label: string; kind: ToolKind; status: "running" | "done" | "failed" };
export type Usage = { input: number; cached: number; output: number; reasoning: number };
export type PaneStatus = "idle" | "running" | "done" | "error" | "aborted" | "interrupted";
export type PaneState = {
  status: PaneStatus;
  mode: string | null;
  label: string | null;
  tools: Tool[];
  answer: string;
  usage: Usage | null;
  ms: number | null;
  error: string | null;
};

export const emptyPane = (): PaneState => ({
  status: "idle", mode: null, label: null, tools: [], answer: "", usage: null, ms: null, error: null,
});

// Codex runs shell tools as `/bin/zsh -lc '<command>'`.
export const unwrapShell = (cmd: string): string => {
  const m = /^(?:\S*\/)?(?:ba|z|da)?sh\s+-l?c\s+(['"])([\s\S]*)\1\s*$/.exec(cmd.trim());
  return m ? m[2]! : cmd.trim();
};

const SEARCH = /^(?:rg|grep|egrep|fgrep|ag|ack|find|fd|git\s+(?:grep|log\s+-S)|pixel\s+(?:search\S*|find\S*|who-calls|impact|concept\S*))\b/;
const READ = /^(?:cat|sed|head|tail|nl|bat|less|more|awk|pixel\s+(?:read\S*|pack-context))\b/;

export const classify = (cmd: string): ToolKind => {
  const segments = unwrapShell(cmd).split(/\s*(?:\|\||&&|\||;)\s*/).map((s) => s.trim());
  if (segments.some((s) => SEARCH.test(s))) return "search";
  if (segments.some((s) => READ.test(s))) return "read";
  return "other";
};

const clip = (s: string, n = 200) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);
const NOISE = /ignoring \d+ unrecognized configuration|dangerously-bypass-hook-trust/;

type Item = Record<string, unknown>;
const toolOf = (item: Item): Pick<Tool, "label" | "kind"> | null => {
  switch (item.type) {
    case "command_execution": {
      const command = unwrapShell(String(item.command ?? ""));
      return { label: command, kind: classify(command) };
    }
    case "mcp_tool_call":
      return { label: `${item.server}.${item.tool}(${clip(JSON.stringify(item.arguments ?? {}), 120)})`, kind: "other" };
    case "web_search":
      return { label: `web_search ${item.query ?? ""}`, kind: "search" };
    case "file_change": {
      const paths = Array.isArray(item.changes) ? item.changes.map((c) => (c as Item).path) : [];
      return { label: `patch ${paths.join(", ")}`, kind: "other" };
    }
    default:
      return null;
  }
};

/** Fold one decoded JSONL event into the pane state. */
export const applyEvent = (s: PaneState, ev: Record<string, unknown>): PaneState => {
  const type = String(ev.type ?? "");
  const item = (ev.item ?? null) as Item | null;
  if (item && (type === "item.started" || type === "item.updated" || type === "item.completed")) {
    if (item.type === "agent_message") {
      if (type === "item.completed") s.answer = String(item.text ?? "");
    } else if (item.type === "error") {
      const msg = String(item.message ?? "");
      if (!NOISE.test(msg)) s.error = clip(msg, 300);
    } else {
      const t = toolOf(item);
      if (t) {
        const id = String(item.id ?? s.tools.length);
        const failed = item.status === "failed" || (typeof item.exit_code === "number" && item.exit_code !== 0);
        const status: Tool["status"] = type === "item.completed" ? (failed ? "failed" : "done") : "running";
        const known = s.tools.find((x) => x.id === id);
        if (known) Object.assign(known, t, { status });
        else s.tools.push({ id, ...t, status });
      }
    }
  } else if (type === "turn.completed") {
    const u = (ev.usage ?? {}) as Record<string, number>;
    const prev = s.usage ?? { input: 0, cached: 0, output: 0, reasoning: 0 };
    s.usage = {
      input: prev.input + (u.input_tokens ?? 0),
      cached: prev.cached + (u.cached_input_tokens ?? 0),
      output: prev.output + (u.output_tokens ?? 0),
      reasoning: prev.reasoning + (u.reasoning_output_tokens ?? 0),
    };
  } else if (type === "turn.failed" || type === "error") {
    const e = (ev.error ?? ev) as Item;
    s.error = clip(String(e.message ?? "run failed"), 300);
  }
  return s;
};

export const paneStats = (s: PaneState) => ({
  calls: s.tools.length,
  searches: s.tools.filter((t) => t.kind === "search").length,
  reads: s.tools.filter((t) => t.kind === "read").length,
  tokens: s.usage ? s.usage.input + s.usage.output : null,
});
