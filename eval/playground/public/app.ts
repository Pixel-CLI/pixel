// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Browser side of the brief playground: vanilla DOM, no framework. All text
// from the API is inserted with textContent, never as HTML.
import type { Decision, ParsedBrief, Range } from "../src/brief";

type Prompt = {
  id: string; text: string; lang: string; on_topic: boolean | null;
  kind: string | null; source: string | null; local: boolean;
};
type BinInfo = { name: string; path: string; exists: boolean; json: boolean | null };
type BriefResult = {
  ok: boolean; error?: string; raw?: string; json?: boolean; note?: string;
  brief: string | null; bytes: number; cap_bytes: number; latency_ms: number;
  parsed: ParsedBrief | null; decision: Decision | null;
};

type Child = Node | string | null | undefined;
const h = <K extends keyof HTMLElementTagNameMap>(
  tag: K, props: Partial<Record<string, unknown>> = {}, ...kids: Child[]
): HTMLElementTagNameMap[K] => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") el.className = String(v);
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v as EventListener);
    else if (v !== false && v != null) el.setAttribute(k, String(v));
  }
  for (const kid of kids) if (kid != null) el.append(kid);
  return el;
};
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const store = {
  get: (k: string) => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* optional */ } },
};
const api = async <T>(url: string, init?: RequestInit): Promise<T> => (await fetch(url, init)).json() as Promise<T>;

const state = {
  prompts: [] as Prompt[],
  bins: [] as BinInfo[],
  selected: null as Prompt | null,
  compare: false,
  binSel: [store.get("binA") ?? "", store.get("binB") ?? ""],
  results: [null, null] as (BriefResult | "loading" | null)[],
};

// ---- prompt list -----------------------------------------------------------

const unique = (xs: (string | null)[]) => [...new Set(xs.filter((x): x is string => !!x))].sort();
const fillSelect = (sel: HTMLSelectElement, values: string[]) => {
  const first = sel.options[0]!;
  sel.replaceChildren(first, ...values.map((v) => h("option", { value: v }, v)));
};

const visible = () => {
  const q = $<HTMLInputElement>("search").value.toLowerCase();
  const kind = $<HTMLSelectElement>("f-kind").value;
  const topic = $<HTMLSelectElement>("f-topic").value;
  const lang = $<HTMLSelectElement>("f-lang").value;
  return state.prompts.filter(
    (p) =>
      (!q || p.text.toLowerCase().includes(q) || p.id.includes(q)) &&
      (!kind || p.kind === kind) &&
      (!topic || String(p.on_topic) === topic) &&
      (!lang || p.lang === lang),
  );
};

const renderList = () => {
  const rows = visible();
  $("count").textContent = `${rows.length} / ${state.prompts.length} prompts`;
  $("prompt-list").replaceChildren(
    ...rows.map((p) =>
      h("li", { class: p.id === state.selected?.id ? "sel" : "", onclick: () => select(p) },
        h("div", {},
          h("span", { class: `tag ${p.on_topic ? "on" : "off"}` }, p.on_topic === null ? "?" : p.on_topic ? "on" : "off"),
          h("span", { class: "tag" }, p.lang),
          p.kind ? h("span", { class: "tag" }, p.kind) : null,
          h("span", { class: "muted" }, p.id)),
        h("div", {}, p.text)),
    ),
  );
};

// ---- brief panel -----------------------------------------------------------

const openRange = async (r: Range) => {
  const res = await api<{ ok: boolean; error?: string; path: string; lines: { n: number; text: string }[]; start: number; end: number }>(
    `/api/file?${new URLSearchParams({ path: r.path, start: String(r.start), end: String(r.end) })}`,
  );
  const body = $("viewer-body");
  if (!res.ok) body.textContent = res.error ?? "error";
  else body.replaceChildren(
    h("strong", {}, `${res.path}:${r.start}${r.end > r.start ? `-${r.end}` : ""}\n`),
    ...res.lines.map((l) =>
      h("span", { class: l.n >= res.start && l.n <= res.end ? "hit" : "" }, `${String(l.n).padStart(5)}  ${l.text}\n`)),
  );
  $<HTMLDialogElement>("viewer").showModal();
};

const chip = (r: Range, cls: string) =>
  h("span", { class: `chip ${cls}`, title: r.label || undefined, onclick: () => void openRange(r) },
    `${r.path}:${r.start}${r.end > r.start ? `-${r.end}` : ""}`);

const renderBrief = (p: ParsedBrief) => {
  const pre = h("div", { class: "brief" });
  for (const { tag, text } of p.lines) {
    const ln = h("span", { class: "ln" });
    const key = (t: string) => h("span", { class: "k" }, `${t}: `);
    if (tag === "header") ln.append(h("span", { class: "hdr" }, text));
    else if (tag === "read" || tag === "also") {
      const r = p.read.concat(p.also).find((x) => text.startsWith(`${x.path}:${x.start}`));
      ln.append(key(tag), r ? chip(r, tag) : text, r?.label ? ` — ${r.label}` : "");
    } else if (tag === "files") {
      ln.append(key(tag));
      for (const r of p.files.filter((x) => text.includes(`${x.path}:${x.start}`))) ln.append(chip(r, "file"));
    } else if (tag === "searched") ln.append(key(tag), h("span", { class: "searched" }, text));
    else if (tag === "confidence") {
      ln.append(key(tag), h("span", { class: `conf-${p.confidenceLevel ?? ""}` }, text));
    } else if (tag === "text") ln.append(text);
    else ln.append(key(tag), text);
    pre.append(ln);
  }
  return pre;
};

const GATE_FEATURES = ["struct_ratio", "struct_per_mille", "best_structural_weight", "total_weight"];
const fmt = (v: unknown) => (typeof v === "number" ? (Number.isInteger(v) ? String(v) : v.toFixed(3)) : String(v));

const renderWhy = (d: Decision) => {
  const f = d.features ?? {};
  const rows: [string, string][] = [
    ["tier", d.tier ?? "none"], ["gate", d.gate ?? "-"], ["score", d.score == null ? "-" : d.score.toFixed(3)],
    ["signal", d.signal ?? "-"], ["kind", d.kind ?? "-"], ["receipt", String(d.receipt)], ["route", d.route ?? "-"],
    ...(d.reason ? [["reason", d.reason] as [string, string]] : []),
    ...(d.bestFile ? [["best file", d.bestFile] as [string, string]] : []),
    ...GATE_FEATURES.filter((k) => k in f).map((k): [string, string] => [k, fmt(f[k])]),
  ];
  const others = Object.entries(f).filter(([k]) => !GATE_FEATURES.includes(k));
  return h("div", { class: "why" },
    h("strong", {}, "why "),
    h("span", { class: `pill ${d.gate === "open" ? "open" : d.gate === "closed" ? "closed" : "other"}` }, d.gate ?? "no gate"),
    h("table", {}, ...rows.map(([k, v]) => h("tr", {}, h("td", {}, k), h("td", {}, v)))),
    others.length ? h("details", {}, h("summary", { class: "muted" }, `${others.length} other features`),
      h("table", {}, ...others.map(([k, v]) => h("tr", {}, h("td", {}, k), h("td", {}, fmt(v)))))) : null);
};

const renderResult = (r: BriefResult | "loading" | null): Node => {
  if (r === null) return h("div", { class: "muted" }, "no run yet");
  if (r === "loading") return h("div", { class: "muted" }, "running...");
  if (!r.ok) return h("div", { class: "err" }, r.error ?? "error", r.raw ? h("pre", {}, r.raw) : null);
  const pct = Math.min(100, Math.round((r.bytes / r.cap_bytes) * 100));
  return h("div", {},
    h("div", { class: "metrics" },
      h("span", {}, `latency ${r.latency_ms} ms${r.decision?.elapsedMs != null ? ` (binary ${r.decision.elapsedMs} ms)` : ""}`),
      h("span", {}, `${r.bytes} / ${r.cap_bytes} B `, h("span", { class: "bar" }, h("i", { class: r.bytes > r.cap_bytes ? "over" : "", style: `width:${pct}%` })))),
    r.note ? h("div", { class: "note" }, r.note) : null,
    r.brief && r.parsed ? renderBrief(r.parsed) : h("div", { class: "muted" }, "no brief (empty output)"),
    r.decision ? renderWhy(r.decision) : null);
};

const renderColumns = () => {
  const n = state.compare ? 2 : 1;
  $("columns").replaceChildren(
    ...[0, 1].slice(0, n).map((i) => {
      const sel = h("select", {
        onchange: (e: Event) => {
          state.binSel[i] = (e.target as HTMLSelectElement).value;
          store.set(i ? "binB" : "binA", state.binSel[i]!);
          void run(i);
        },
      }, ...state.bins.map((b) => h("option", { value: b.path, selected: b.path === state.binSel[i], disabled: !b.exists },
        `${b.name}${b.exists ? "" : " (missing)"}${b.json === false ? " (no --json)" : ""}`)));
      return h("div", { class: "panel" },
        h("div", { class: "col-head" }, h("strong", {}, i ? "B" : "A"), sel,
          h("button", { onclick: () => void run(i) }, "rerun")),
        renderResult(state.results[i]!));
    }),
  );
};

const run = async (i: number) => {
  const p = state.selected;
  const bin = state.binSel[i];
  if (!p || !bin) return;
  state.results[i] = "loading";
  renderColumns();
  try {
    state.results[i] = await api<BriefResult>("/api/brief", {
      method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ prompt: p.text, bin }),
    });
  } catch (e) {
    state.results[i] = { ok: false, error: String(e) } as BriefResult;
  }
  if (state.selected === p) renderColumns();
};

const select = (p: Prompt) => {
  state.selected = p;
  state.results = [null, null];
  $("current").replaceChildren(h("strong", {}, `${p.id}: `), p.text);
  renderList();
  void run(0);
  if (state.compare) void run(1);
};

// ---- boot ------------------------------------------------------------------

const boot = async () => {
  const [prompts, bins] = await Promise.all([
    api<Prompt[]>("/api/prompts"),
    api<{ repo: string; bins: BinInfo[] }>("/api/bins"),
  ]);
  state.prompts = prompts;
  state.bins = bins.bins;
  $("repo").textContent = `fixture: ${bins.repo}`;
  // Binaries with `brief --json` first: they are the useful default for A.
  const usable = bins.bins.filter((b) => b.exists).sort((a, b) => Number(b.json === true) - Number(a.json === true)).map((b) => b.path);
  state.binSel[0] = usable.includes(state.binSel[0]!) ? state.binSel[0]! : (usable[0] ?? "");
  state.binSel[1] = usable.includes(state.binSel[1]!) ? state.binSel[1]! : (usable[1] ?? usable[0] ?? "");
  fillSelect($("f-kind") as HTMLSelectElement, unique(prompts.map((p) => p.kind)));
  fillSelect($("f-lang") as HTMLSelectElement, unique(prompts.map((p) => p.lang)));
  for (const id of ["search", "f-kind", "f-topic", "f-lang"]) $(id).addEventListener("input", renderList);
  $<HTMLInputElement>("compare").addEventListener("change", (e) => {
    state.compare = (e.target as HTMLInputElement).checked;
    renderColumns();
    if (state.compare && state.selected && !state.results[1]) void run(1);
  });
  $<HTMLFormElement>("add-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const f = new FormData(e.target as HTMLFormElement);
    const res = await api<{ ok: boolean; prompt: Prompt; error?: string }>("/api/prompts", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify({ text: f.get("text"), lang: f.get("lang"), kind: f.get("kind"), on_topic: f.get("on_topic") === "on" }),
    });
    if (!res.ok) return void alert(res.error);
    state.prompts.push(res.prompt);
    (e.target as HTMLFormElement).reset();
    renderList();
    select(res.prompt);
  });
  renderList();
  renderColumns();
};

void boot();
