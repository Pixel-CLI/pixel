// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Parsing of the `[PIXEL:BRIEF]` text and the decision record that
// `pixel brief --json` prints. Pure functions: no I/O.

export const BRIEF_CAP_BYTES = 2048;

export type Range = { path: string; start: number; end: number; label: string };

export type ParsedBrief = {
  header: boolean;
  kind: string | null;
  anchors: string | null;
  searched: string | null;
  confidence: string | null;
  confidenceLevel: string | null;
  read: Range[];
  also: Range[];
  files: Range[];
  unresolved: string[];
  coverage: string | null;
  next: string | null;
  lines: { tag: string; text: string }[];
};

const RANGE = /^(.+?):(\d+)(?:-(\d+))?(?:\s+—\s+(.*))?$/s;

export const parseRange = (raw: string): Range | null => {
  const m = RANGE.exec(raw.trim());
  if (!m) return null;
  const start = Number(m[2]);
  return { path: m[1]!, start, end: m[3] ? Number(m[3]) : start, label: (m[4] ?? "").trim() };
};

// `files:` packs entries as `path:line — snippet; path:line — snippet (+6 more)`.
const splitFiles = (body: string): Range[] =>
  body
    .replace(/\s*\(\+\d+ more\)\s*$/, "")
    .split(/;\s+(?=[\w./-]+:\d+)/)
    .map(parseRange)
    .filter((r): r is Range => r !== null);

export const parseBrief = (text: string): ParsedBrief => {
  const out: ParsedBrief = {
    header: false,
    kind: null,
    anchors: null,
    searched: null,
    confidence: null,
    confidenceLevel: null,
    read: [],
    also: [],
    files: [],
    unresolved: [],
    coverage: null,
    next: null,
    lines: [],
  };
  for (const line of text.split("\n")) {
    const m = /^([a-z][a-z ()]*):\s?(.*)$/.exec(line);
    if (line.startsWith("[PIXEL:BRIEF]")) {
      out.header = true;
      out.lines.push({ tag: "header", text: line });
    } else if (m) {
      const [key, body] = [m[1]!, m[2]!];
      out.lines.push({ tag: key, text: body });
      if (key === "kind") out.kind = body;
      else if (key === "anchors") out.anchors = body;
      else if (key === "searched") out.searched = body;
      else if (key === "confidence") {
        out.confidence = body;
        out.confidenceLevel = /^(\w+)/.exec(body)?.[1] ?? null;
      } else if (key === "read" || key === "also") {
        const r = parseRange(body);
        if (r) out[key].push(r);
      } else if (key === "files") out.files.push(...splitFiles(body));
      else if (key === "unresolved") out.unresolved.push(body);
      else if (key === "coverage") out.coverage = body;
      else if (key === "next") out.next = body;
    } else out.lines.push({ tag: "text", text: line });
  }
  return out;
};

export type Decision = {
  tier: string | null;
  score: number | null;
  gate: string | null;
  signal: string | null;
  kind: string | null;
  reason: string | null;
  receipt: boolean | null;
  route: string | null;
  elapsedMs: number | null;
  features: Record<string, number | boolean> | null;
  bestFile: string | null;
};

const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" ? v : null);

// The record is whatever the binary prints; every field is optional so an
// older or newer binary still renders.
export const decisionOf = (rec: Record<string, unknown>): Decision => ({
  tier: str(rec.tier),
  score: num(rec.score),
  gate: str(rec.gate),
  signal: str(rec.signal),
  kind: str(rec.kind),
  reason: str(rec.reason),
  receipt: typeof rec.receipt === "boolean" ? rec.receipt : null,
  route: str(rec.route),
  elapsedMs: num(rec.elapsed_ms),
  features:
    rec.features && typeof rec.features === "object"
      ? (rec.features as Record<string, number | boolean>)
      : null,
  bestFile: str(rec.best_file),
});

// stdout may carry a leading or trailing non-JSON line; take the first object.
export const parseJsonRecord = (stdout: string): Record<string, unknown> | null => {
  const t = stdout.trim();
  const candidates = [t, ...t.split("\n").filter((l) => l.startsWith("{"))];
  for (const c of candidates) {
    try {
      const v = JSON.parse(c);
      if (v && typeof v === "object" && !Array.isArray(v)) return v as Record<string, unknown>;
    } catch {
      // try the next candidate
    }
  }
  return null;
};
