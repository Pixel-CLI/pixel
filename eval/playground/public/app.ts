// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Browser side of the brief playground: a strip of test cards, vanilla DOM.
// All text from the API is inserted with textContent, never as HTML.
import type { Decision, ParsedBrief, Range } from "../src/brief";
import { mountCensus } from "./census";
import { applyEvent, emptyPane, paneStats, type PaneState } from "../src/events";

type Prompt = { id: string; text: string; lang: string; on_topic: boolean | null; kind: string | null };
type BinInfo = { name: string; path: string; exists: boolean; json: boolean | null };
type BriefResult = {
  ok: boolean; error?: string; raw?: string; json?: boolean; note?: string;
  brief: string | null; bytes: number; cap_bytes: number; latency_ms: number;
  parsed: ParsedBrief | null; decision: Decision | null; graph_status?: GraphStatus;
};
type Fresh = "ready" | "stale" | "missing" | "unknown";
type GraphStatus = {
  graph: Fresh; index: Fresh; detail: string; checked_at: string;
  graph_age_secs: number | null; index_age_secs: number | null; source: string;
};
type StatusResp = GraphStatus & {
  ok: boolean; error?: string;
  refresh?: { state: "idle" | "running" | "done" | "error"; bin: string | null; error: string | null };
};
type Arm = "pixel" | "none";
type Card = {
  id: string; prompt: string; bin: string; sentPrompt: string;
  brief: BriefResult | "loading" | null; panes: Record<Arm, PaneState>; whyOpen: boolean;
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
const post = (body: unknown): RequestInit => ({
  method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body),
});

let corpus: Prompt[] = [];
let bins: BinInfo[] = [];
const cards: Card[] = [];
const views = new Map<string, { updateBrief: () => void; updatePane: (a: Arm) => void; updateBadge: () => void; root: HTMLElement }>();

// ---- graph status ----------------------------------------------------------

const statuses = new Map<string, StatusResp>();
const pollers = new Map<string, ReturnType<typeof setInterval>>();
const notify = () => views.forEach((v) => v.updateBadge());
const age = (secs: number | null) =>
  secs == null ? "age unknown" : secs < 90 ? `${secs} s old` : secs < 5400 ? `${Math.round(secs / 60)} min old` : secs < 172800 ? `${Math.round(secs / 3600)} h old` : `${Math.round(secs / 86400)} d old`;
const loadStatus = async (bin: string) => {
  if (!bin) return;
  try {
    statuses.set(bin, await api<StatusResp>(`/api/status?${new URLSearchParams({ bin })}`));
  } catch (e) {
    statuses.set(bin, { ok: false, error: String(e) } as StatusResp);
  }
  notify();
};
const poll = (bin: string) => {
  if (pollers.has(bin)) return;
  pollers.set(bin, setInterval(async () => {
    await loadStatus(bin);
    const st = statuses.get(bin);
    if (!st?.ok || st.refresh?.state !== "running") { clearInterval(pollers.get(bin)); pollers.delete(bin); }
  }, 2000));
};
const refreshGraph = async (bin: string) => {
  const r = await api<{ ok: boolean; error?: string }>("/api/refresh", post({ bin }));
  if (!r.ok) return void alert(r.error);
  await loadStatus(bin);
  poll(bin);
};
const controllers = new Map<string, AbortController[]>();

// ---- persistence -----------------------------------------------------------

const KEY = "brief-playground.cards.v2";
let saveTimer: ReturnType<typeof setTimeout> | undefined;
const save = () => {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => store.set(KEY, JSON.stringify(cards.map((c) => ({ ...c, brief: c.brief === "loading" ? null : c.brief })))), 150);
};
const load = (): Card[] => {
  try {
    const raw = JSON.parse(store.get(KEY) ?? "[]") as Card[];
    for (const c of raw) for (const arm of ["pixel", "none"] as const) {
      // A reload cannot resume a stream: say so rather than show a spinner.
      if (c.panes[arm].status === "running") c.panes[arm].status = "interrupted";
    }
    return raw;
  } catch { return []; }
};

const newCard = (): Card => ({
  id: `card-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 5)}`,
  prompt: "", bin: bins.find((b) => b.exists && b.json)?.path ?? bins.find((b) => b.exists)?.path ?? "",
  sentPrompt: "", brief: null, panes: { pixel: emptyPane(), none: emptyPane() }, whyOpen: false,
});

const badgeEl = (st: StatusResp | GraphStatus | undefined, refreshing = false) => {
  const state = refreshing ? "refreshing" : (st && "ok" in st && !st.ok ? "unknown" : (st?.graph ?? "unknown"));
  const cls = state === "ready" ? "ok" : state === "stale" ? "warn" : state === "missing" ? "bad" : "dim";
  const tip = st && !("ok" in st && !st.ok)
    ? `${st.detail}\ngraph ${age(st.graph_age_secs)}, index ${st.index} (${age(st.index_age_secs)})\nchecked ${st.checked_at} via ${st.source}`
    : st && "error" in st ? String(st.error) : "not checked yet";
  return h("span", { class: `badge ${cls}`, title: tip }, `graph: ${state}`);
};

// ---- brief rendering -------------------------------------------------------

const rangeText = (r: Range) => `${r.path}:${r.start}${r.end > r.start ? `-${r.end}` : ""}`;
const openRange = async (r: Range) => {
  const res = await api<{ ok: boolean; error?: string; path: string; lines: { n: number; text: string }[]; start: number; end: number }>(
    `/api/file?${new URLSearchParams({ path: r.path, start: String(r.start), end: String(r.end) })}`,
  );
  const body = $("viewer-body");
  if (!res.ok) body.textContent = res.error ?? "error";
  else body.replaceChildren(
    h("strong", {}, `${rangeText(r)}\n`),
    ...res.lines.map((l) => h("span", { class: l.n >= res.start && l.n <= res.end ? "hit" : "" }, `${String(l.n).padStart(5)}  ${l.text}\n`)),
  );
  $<HTMLDialogElement>("viewer").showModal();
};
const chip = (r: Range, cls: string) =>
  h("span", { class: `chip ${cls}`, title: r.label || undefined, onclick: () => void openRange(r) }, rangeText(r));

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
    else if (tag === "confidence") ln.append(key(tag), h("span", { class: `conf-${p.confidenceLevel ?? ""}` }, text));
    else if (tag === "text") ln.append(text);
    else ln.append(key(tag), text);
    pre.append(ln);
  }
  return pre;
};

const GATE_FEATURES = ["struct_ratio", "struct_per_mille", "best_structural_weight", "total_weight"];
const fmt = (v: unknown) => (typeof v === "number" ? (Number.isInteger(v) ? String(v) : v.toFixed(3)) : String(v));
const renderWhy = (card: Card, r: BriefResult) => {
  const d = r.decision;
  const f = d?.features ?? {};
  const rows: [string, string][] = [
    ["tier", d?.tier ?? "none"], ["gate", d?.gate ?? "-"], ["score", d?.score == null ? "-" : d.score.toFixed(3)],
    ["signal", d?.signal ?? "-"], ["kind", d?.kind ?? "-"], ["receipt", String(d?.receipt ?? "-")],
    ["bytes", `${r.bytes} / ${r.cap_bytes}`], ["latency", `${r.latency_ms} ms${d?.elapsedMs != null ? ` (binary ${d.elapsedMs} ms)` : ""}`],
    ...(d?.reason ? [["reason", d.reason] as [string, string]] : []),
    ...(d?.bestFile ? [["best file", d.bestFile] as [string, string]] : []),
    ...Object.entries(f).sort(([a], [b]) => Number(!GATE_FEATURES.includes(a)) - Number(!GATE_FEATURES.includes(b)))
      .map(([k, v]): [string, string] => [k, fmt(v)]),
  ];
  const det = h("details", { class: "why", open: card.whyOpen },
    h("summary", {}, "why ",
      h("span", { class: `pill ${d?.gate === "open" ? "open" : d?.gate === "closed" ? "closed" : "other"}` }, d?.gate ?? "no record")),
    h("table", {}, ...rows.map(([k, v]) => h("tr", {}, h("td", {}, k), h("td", {}, v)))));
  det.addEventListener("toggle", () => { card.whyOpen = det.open; save(); });
  return det;
};

// ---- panes -----------------------------------------------------------------

const PANE_TITLE: Record<Arm, string> = { pixel: "Codex with the brief", none: "Codex without the brief (initial prompt only)" };
const secs = (ms: number | null) => (ms == null ? "-" : `${(ms / 1000).toFixed(1)} s`);

const renderPane = (card: Card, arm: Arm) => {
  const s = card.panes[arm];
  const stats = paneStats(s);
  const stream = h("div", { class: "stream" },
    s.status === "idle" ? h("div", { class: "muted" }, "waiting for Test") : null,
    ...s.tools.map((t, i) => h("div", { class: `tool ${t.status}` },
      h("span", { class: "n" }, String(i + 1)), h("span", { class: "s" }, t.status === "running" ? "…" : t.status === "done" ? "✓" : "✗"),
      h("span", { class: "t" }, t.label))),
    s.status === "running" && !s.answer ? h("div", { class: "muted" }, s.tools.length ? "working…" : "starting Codex…") : null,
    s.answer ? h("div", { class: "answer" }, s.answer) : null,
    s.error ? h("div", { class: "err" }, s.error) : null);
  const u = s.usage;
  return h("div", { class: "pane" },
    h("h2", {}, h("span", { class: "grow" }, PANE_TITLE[arm]), s.label ? h("span", { class: "tag" }, s.label) : null,
      h("span", { class: "tag" }, s.status), h("span", { class: "tag" }, `${stats.calls} calls`)),
    stream,
    h("footer", {},
      h("span", {}, `${stats.calls} tool calls`), h("span", {}, `${stats.searches} searches / ${stats.reads} reads`),
      h("span", {}, secs(s.ms)),
      h("span", {}, u ? `${u.input + u.output} tokens (in ${u.input}, cached ${u.cached}, out ${u.output})` : "tokens -")));
};

const stream = async (card: Card, arm: Arm, signal: AbortSignal) => {
  const s: PaneState = (card.panes[arm] = { ...emptyPane(), status: "running" });
  const t0 = performance.now();
  const view = () => views.get(card.id)?.updatePane(arm);
  view();
  const tick = setInterval(() => { s.ms = Math.round(performance.now() - t0); view(); }, 1000);
  try {
    const res = await fetch("/api/run", { ...post({ cardId: card.id, prompt: card.sentPrompt, arm, briefText: arm === "pixel" ? (card.brief as BriefResult).brief ?? "" : "" }), signal });
    if (!res.ok || !res.body) {
      s.status = "error";
      s.error = ((await res.json().catch(() => null)) as { error?: string } | null)?.error ?? `HTTP ${res.status}`;
      return;
    }
    const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
    let buf = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += value;
      let i: number;
      while ((i = buf.indexOf("\n\n")) >= 0) {
        const frame = buf.slice(0, i);
        buf = buf.slice(i + 2);
        const event = /^event: (.*)$/m.exec(frame)?.[1];
        const data = frame.split("\n").filter((l) => l.startsWith("data: ")).map((l) => l.slice(6)).join("\n");
        try {
          if (event === "meta") Object.assign(s, { mode: JSON.parse(data).mode, label: JSON.parse(data).label });
          else if (event === "codex") applyEvent(s, JSON.parse(data));
          else if (event === "end") {
            const end = JSON.parse(data) as { code: number | null; aborted: boolean; ms: number; stderr: string };
            s.ms = end.ms;
            s.status = end.aborted ? "aborted" : end.code === 0 && !s.error ? "done" : "error";
            if (s.status === "error" && !s.error) s.error = end.stderr || `codex exited with ${end.code}`;
          }
        } catch { /* a malformed line is skipped */ }
        view();
        save();
      }
    }
    if (s.status === "running") s.status = signal.aborted ? "aborted" : "done";
  } catch (e) {
    s.status = signal.aborted ? "aborted" : "error";
    if (!signal.aborted) s.error = String(e);
  } finally {
    clearInterval(tick);
    view();
    save();
  }
};

// ---- card ------------------------------------------------------------------

const send = async (card: Card) => {
  if (!card.prompt.trim() || !card.bin) return;
  abort(card);
  card.sentPrompt = card.prompt.trim();
  card.brief = "loading";
  card.panes = { pixel: emptyPane(), none: emptyPane() };
  views.get(card.id)?.updateBrief();
  for (const a of ["pixel", "none"] as const) views.get(card.id)?.updatePane(a);
  try {
    card.brief = await api<BriefResult>("/api/brief", post({ prompt: card.sentPrompt, bin: card.bin }));
  } catch (e) {
    card.brief = { ok: false, error: String(e) } as BriefResult;
  }
  views.get(card.id)?.updateBrief();
  save();
};

const abort = (card: Card) => {
  for (const c of controllers.get(card.id) ?? []) c.abort();
  controllers.delete(card.id);
  void fetch("/api/abort", post({ cardId: card.id }));
};

const test = (card: Card) => {
  abort(card);
  const ctl = [new AbortController(), new AbortController()];
  controllers.set(card.id, ctl);
  void stream(card, "pixel", ctl[0]!.signal);
  void stream(card, "none", ctl[1]!.signal);
};

const mountCard = (card: Card, index: number): HTMLElement => {
  const input = h("textarea", { class: "prompt", rows: "1", placeholder: "type the prompt to test" });
  input.value = card.prompt;
  const sendBtn = h("button", { class: "primary", onclick: () => void send(card) }, "Send");
  input.addEventListener("input", () => { card.prompt = input.value; save(); });
  input.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void send(card); });

  const search = h("input", { type: "search", placeholder: "pick from corpus: filter" });
  const pick = h("select", {});
  const fillPick = () => {
    const q = search.value.toLowerCase();
    const rows = corpus.filter((p) => !q || p.text.toLowerCase().includes(q) || p.id.includes(q)).slice(0, 300);
    pick.replaceChildren(h("option", { value: "" }, `${rows.length} prompts…`),
      ...rows.map((p) => h("option", { value: p.id }, `${p.id} ${p.on_topic ? "" : "(off) "}${p.text.slice(0, 110)}`)));
  };
  search.addEventListener("input", fillPick);
  pick.addEventListener("change", () => {
    const p = corpus.find((x) => x.id === pick.value);
    if (p) { input.value = card.prompt = p.text; save(); }
  });
  fillPick();

  const binSel = h("select", { title: "pixel binary", onchange: (e: Event) => { card.bin = (e.target as HTMLSelectElement).value; save(); updateBadge(); void loadStatus(card.bin); } },
    ...bins.map((b) => h("option", { value: b.path, selected: b.path === card.bin, disabled: !b.exists },
      `${b.name}${b.exists ? "" : " (missing)"}${b.json === false ? " (no --json)" : ""}`)));

  const badge = h("span", {});
  const refreshBtn = h("button", { title: "run prepare-repo in the fixture (one at a time)", onclick: () => void refreshGraph(card.bin) }, "refresh");
  const updateBadge = () => {
    const st = statuses.get(card.bin);
    const running = st?.refresh?.state === "running";
    badge.replaceChildren(badgeEl(st, running));
    refreshBtn.disabled = running || !card.bin;
    refreshBtn.title = st?.refresh?.state === "error" ? `last refresh failed: ${st.refresh.error}` : "run prepare-repo in the fixture (one at a time)";
  };
  const briefBox = h("div", { class: "box briefbox" });
  const testBtn = h("button", { class: "primary", onclick: () => test(card) }, "Test");
  const abortBtn = h("button", { class: "danger", onclick: () => abort(card), title: "kill both Codex runs of this card" }, "Abort");
  const panes = { pixel: h("div", { style: "display:contents" }), none: h("div", { style: "display:contents" }) };
  const paneGrid = h("div", { class: "panes" }, panes.pixel, panes.none);

  const updateBrief = () => {
    const r = card.brief;
    const ready = r !== null && r !== "loading" && r.ok;
    const body: Child[] = [];
    if (r === null) body.push(h("div", { class: "muted" }, "Send a prompt to build its brief."));
    else if (r === "loading") body.push(h("div", { class: "muted" }, "building the brief…"));
    else if (!r.ok) body.push(h("div", { class: "err" }, r.error ?? "error"), r.raw ? h("pre", {}, r.raw) : null);
    else {
      const stale = r.brief ? /graph (is )?stale/i.test(r.brief) : false;
      body.push(r.note ? h("div", { class: "note" }, r.note) : null,
        stale ? h("div", { class: "note" }, "⚠ this brief was built on a stale graph") : null,
        r.graph_status ? h("div", { class: "muted" }, "graph when built: ", badgeEl(r.graph_status)) : null,
        r.brief && r.parsed ? renderBrief(r.parsed) : h("div", { class: "muted" }, "no brief for this prompt (empty output): the pixel arm would get nothing"),
        renderWhy(card, r));
    }
    briefBox.replaceChildren(
      h("h2", {}, h("span", { class: "grow" }, "Pixel brief ready to hydrate the LLM"),
        ready ? h("span", { class: "muted" }, `${(r as BriefResult).bytes} / ${(r as BriefResult).cap_bytes} B · ${(r as BriefResult).latency_ms} ms`) : null,
        ready ? testBtn : null),
      ...body.filter((x): x is Node | string => x != null));
    testBtn.disabled = !(ready && (r as BriefResult).brief);
    sendBtn.disabled = r === "loading";
  };
  const updatePane = (arm: Arm) => {
    const old = panes[arm].querySelector(".stream") as HTMLElement | null;
    const atBottom = !old || old.scrollHeight - old.scrollTop - old.clientHeight < 24;
    panes[arm].replaceChildren(renderPane(card, arm));
    const now = panes[arm].querySelector(".stream") as HTMLElement;
    if (old && !atBottom) now.scrollTop = old.scrollTop;
    else now.scrollTop = now.scrollHeight;
    const running = card.panes.pixel.status === "running" || card.panes.none.status === "running";
    abortBtn.disabled = !running;
  };

  const root = h("section", { class: "card", "data-card": card.id },
    h("div", { class: "bar" }, h("strong", {}, `Test ${index + 1}`), h("span", { class: "muted grow" }, ""), badge, refreshBtn, binSel, abortBtn,
      h("button", { title: "remove this card", onclick: () => removeCard(card) }, "×")),
    h("div", { class: "bar" }, input, sendBtn),
    h("div", { class: "corpus" }, search, pick),
    briefBox, paneGrid);
  views.set(card.id, { updateBrief, updatePane, updateBadge, root });
  updateBrief();
  updateBadge();
  void loadStatus(card.bin);
  updatePane("pixel");
  updatePane("none");
  return root;
};

const removeCard = (card: Card) => {
  abort(card);
  cards.splice(cards.indexOf(card), 1);
  views.delete(card.id);
  render();
  save();
};

const addCard = () => {
  cards.push(newCard());
  render();
  save();
  requestAnimationFrame(() => {
    const last = $("strip").querySelectorAll(".card:not(.new)");
    last[last.length - 1]?.scrollIntoView({ behavior: "smooth", inline: "start" });
  });
};

const render = () => {
  views.clear();
  $("strip").replaceChildren(
    ...cards.map(mountCard),
    h("section", { class: "card new", onclick: addCard }, "+ new test"),
  );
};

const boot = async () => {
  [corpus, { bins }] = await Promise.all([
    api<Prompt[]>("/api/prompts"),
    api<{ repo: string; bins: BinInfo[] }>("/api/bins"),
  ]);
  cards.push(...load());
  if (!cards.length) cards.push(newCard());
  render();
};

// Header tabs: the test cards and the census view.
let censusMounted = false;
for (const btn of document.querySelectorAll<HTMLButtonElement>("#tabs button")) {
  btn.addEventListener("click", () => {
    const tab = btn.dataset.tab;
    for (const b of document.querySelectorAll("#tabs button")) b.classList.toggle("on", b === btn);
    $("strip").hidden = tab !== "cards";
    $("census").hidden = tab !== "census";
    if (tab === "census" && !censusMounted) { censusMounted = true; mountCensus($("census")); }
    store.set("brief-playground.tab", tab ?? "cards");
  });
}
if (store.get("brief-playground.tab") === "census") document.querySelector<HTMLButtonElement>('#tabs button[data-tab="census"]')?.click();

void boot();
