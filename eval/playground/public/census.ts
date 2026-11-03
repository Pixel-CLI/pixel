// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT
// Census view: what agents do in a session before they answer, from the local
// census.db. Summary charts are plain SVG/CSS. History prompts are private and
// arrive as null: they are shown as "private".

type Group = "search" | "definition" | "usages" | "read" | "list" | "edit" | "test" | "git" | "web" | "other";
const COLORS: Record<Group, string> = {
  search: "#5ea1ff", definition: "#c792ea", usages: "#ff9e64", read: "#56d364", list: "#4dd0e1",
  edit: "#ff7b72", test: "#e3b341", git: "#9aa0a6", web: "#f78fb3", other: "#4a4a47",
};
const CODES: Record<string, Group> = {
  s: "search", d: "definition", u: "usages", r: "read", l: "list", e: "edit", t: "test", g: "git", w: "web", o: "other",
};
const ORDER = Object.keys(COLORS) as Group[];

type Row = {
  id: string; source: string; repo: string | null; category: string | null; mode: string | null;
  prompt: string | null; private: boolean; wall_s: number | null; cost_usd: number | null; tokens: number;
  n_calls: number; explore_before_first_edit_or_answer: number; strip: string;
};
type SessionList = { ok: boolean; error?: string; sessions: Row[]; categories: string[]; repos: string[] };
type Bucket = { bucket: string; count: number };
type Summary = {
  ok: boolean; error?: string; sessions: number; calls: number;
  by_group: { group: Group; n: number }[]; by_class: { class: string; n: number }[];
  calls_per_session: Bucket[]; explore_before: Bucket[];
  by_category: { category: string; sessions: number; avg_calls: number; avg_explore: number }[];
};
type Call = {
  seq: number; tool: string | null; class: string | null; need: string | null; group: Group;
  input_json: string | null; output: string; output_bytes: number; is_error: boolean;
};
type Detail = {
  ok: boolean; error?: string; calls: Call[]; final_answer: string | null; diff: string | null;
  session: Row & { sha: string | null };
};

const SVG = "http://www.w3.org/2000/svg";
const s = (tag: string, attrs: Record<string, string | number> = {}, ...kids: (Node | string)[]) => {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  el.append(...kids);
  return el;
};
type Child = Node | string | null | undefined;
const h = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Record<string, unknown> = {}, ...kids: Child[]) => {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") el.className = String(v);
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v as EventListener);
    else if (v !== false && v != null) el.setAttribute(k, String(v));
  }
  for (const kid of kids) if (kid != null) el.append(kid);
  return el;
};
const nodes = (xs: Child[]) => xs.filter((x): x is Node | string => x != null);
const get = async <T>(url: string): Promise<T> => (await fetch(url)).json() as Promise<T>;

/** One-line argument summary of a call: the pattern, command or file:range. */
export const argLine = (tool: string | null, inputJson: string | null): string => {
  let o: Record<string, unknown> = {};
  try { o = JSON.parse(inputJson ?? "{}"); } catch { return (inputJson ?? "").slice(0, 160); }
  const str = (k: string) => (typeof o[k] === "string" ? (o[k] as string) : null);
  const file = str("file_path") ?? str("path") ?? str("filePath");
  const range = o.offset != null || o.limit != null ? `:${o.offset ?? 1}${o.limit != null ? `+${o.limit}` : ""}` : "";
  const parts = [str("command") ?? str("cmd"), str("pattern") ?? str("query"), file ? `${file}${range}` : null, str("url"), str("description")]
    .filter((x): x is string => !!x);
  const line = parts.length ? parts.join("  ") : JSON.stringify(o);
  return line.length > 200 ? `${line.slice(0, 199)}…` : line;
};

const stripBackground = (strip: string) => {
  if (!strip) return "none";
  const w = 6;
  const stops = [...strip].map((c, i) => `${COLORS[CODES[c] ?? "other"]} ${i * w}px ${(i + 1) * w - 1}px, transparent ${(i + 1) * w - 1}px ${(i + 1) * w}px`);
  return `linear-gradient(to right, ${stops.join(", ")})`;
};
const chip = (g: Group, text: string = g) => h("span", { class: "nchip", style: `background:${COLORS[g]}` }, text);

const histogram = (title: string, data: Bucket[]) => {
  const W = 260, H = 110, pad = 18;
  const max = Math.max(1, ...data.map((d) => d.count));
  const bw = (W - 8) / data.length;
  const svg = s("svg", { viewBox: `0 0 ${W} ${H}`, width: "100%", height: H, role: "img" });
  data.forEach((d, i) => {
    const bh = ((H - pad - 12) * d.count) / max;
    svg.append(
      s("rect", { x: 4 + i * bw + 2, y: H - pad - bh, width: bw - 4, height: bh, fill: "#5ea1ff", rx: 2 }, s("title", {}, `${d.bucket}: ${d.count}`)),
      s("text", { x: 4 + i * bw + bw / 2, y: H - 5, "text-anchor": "middle", fill: "#8d8d86", "font-size": 9 }, d.bucket),
      d.count ? s("text", { x: 4 + i * bw + bw / 2, y: H - pad - bh - 3, "text-anchor": "middle", fill: "#e8e8e3", "font-size": 9 }, String(d.count)) : "",
    );
  });
  return h("div", { class: "cpanel" }, h("h3", {}, title), svg as unknown as Node);
};

const needsChart = (sum: Summary) => {
  const total = sum.by_group.reduce((a, g) => a + g.n, 0) || 1;
  const sorted = [...sum.by_group].sort((a, b) => ORDER.indexOf(a.group) - ORDER.indexOf(b.group));
  const bar = h("div", { class: "stack" }, ...sorted.map((g) =>
    h("span", { style: `width:${(g.n / total) * 100}%;background:${COLORS[g.group]}`, title: `${g.group}: ${g.n} (${((g.n / total) * 100).toFixed(1)}%)` })));
  const legend = h("div", { class: "legend" }, ...sorted.map((g) =>
    h("span", {}, h("i", { style: `background:${COLORS[g.group]}` }), `${g.group} ${((g.n / total) * 100).toFixed(0)}%`)));
  return h("div", { class: "cpanel wide" }, h("h3", {}, `calls by need (${sum.calls.toLocaleString()} calls, ${sum.sessions.toLocaleString()} sessions)`), bar, legend,
    h("div", { class: "muted cats" }, ...sum.by_category.slice(0, 8).map((c) =>
      h("span", {}, `${c.category} ${c.sessions}× avg ${c.avg_calls.toFixed(1)} calls, explore ${c.avg_explore.toFixed(1)}`))));
};

export const mountCensus = (root: HTMLElement) => {
  const st = { source: "census", category: "", repo: "", selected: "", limit: 300, rows: [] as Row[] };
  const top = h("div", { class: "c-top" });
  const filters = h("div", { class: "c-filters" });
  const count = h("div", { class: "muted" });
  const list = h("div", { class: "c-list" });
  const detail = h("div", { class: "c-detail" }, h("div", { class: "muted" }, "pick a session"));
  root.replaceChildren(top, h("div", { class: "c-main" }, h("div", { class: "c-left" }, filters, count, list), detail));

  const legend = h("div", { class: "legend c-legend" }, ...ORDER.map((g) => h("span", {}, h("i", { style: `background:${COLORS[g]}` }), g)));

  const loadSummary = async () => {
    const sum = await get<Summary>(`/api/census/summary?source=${st.source}`);
    top.replaceChildren(...(sum.ok ? [needsChart(sum), histogram("explore calls before the first edit (or answer)", sum.explore_before), histogram("calls per session", sum.calls_per_session)]
      : [h("div", { class: "err" }, sum.error ?? "error")]));
  };

  const renderList = () => {
    count.textContent = `${st.rows.length} sessions`;
    list.replaceChildren(...nodes([legend, ...st.rows.slice(0, st.limit).map((r) =>
      h("div", { class: `c-row${r.id === st.selected ? " sel" : ""}`, onclick: () => void select(r.id) },
        h("div", { class: "line" }, h("span", { class: "tag" }, r.category ?? "-"), h("span", { class: "tag" }, r.mode ?? "-"),
          h("strong", {}, r.repo ?? "?"), h("span", { class: "muted" }, r.id)),
        h("div", { class: r.private ? "muted" : "" }, r.private ? "private" : (r.prompt ?? "").slice(0, 120)),
        h("div", { class: "strip", style: `background:${stripBackground(r.strip)}`, title: `${r.n_calls} calls` }),
        h("div", { class: "muted" }, `explore ${r.explore_before_first_edit_or_answer} · total ${r.n_calls}${r.wall_s != null ? ` · ${r.wall_s.toFixed(0)} s` : ""}${r.cost_usd ? ` · $${r.cost_usd.toFixed(3)}` : ""}`))),
      st.rows.length > st.limit ? h("button", { onclick: () => { st.limit += 300; renderList(); } }, `show ${Math.min(300, st.rows.length - st.limit)} more`) : null]));
  };

  const loadList = async () => {
    const q = new URLSearchParams({ source: st.source, category: st.category, repo: st.repo });
    const res = await get<SessionList>(`/api/census/sessions?${q}`);
    if (!res.ok) { list.replaceChildren(h("div", { class: "err" }, res.error ?? "error")); return; }
    st.rows = res.sessions;
    const sel = (id: string, label: string, value: string, opts: string[]) => {
      const el = h("select", { onchange: (e: Event) => { st[id as "category" | "repo"] = (e.target as HTMLSelectElement).value; st.limit = 300; void loadList(); } },
        h("option", { value: "" }, `all ${label}`), ...opts.map((o) => h("option", { value: o, selected: o === value }, o)));
      return el;
    };
    filters.replaceChildren(
      h("select", { onchange: (e: Event) => { st.source = (e.target as HTMLSelectElement).value; st.category = st.repo = st.selected = ""; st.limit = 300; void refresh(); } },
        ...["census", "claude-history", "codex-history", "pi-history"].map((o) => h("option", { value: o, selected: o === st.source }, o))),
      sel("category", "categories", st.category, res.categories), sel("repo", "repos", st.repo, res.repos));
    renderList();
  };

  const select = async (id: string) => {
    st.selected = id;
    renderList();
    const d = await get<Detail>(`/api/census/session/${encodeURIComponent(id)}`);
    if (!d.ok) { detail.replaceChildren(h("div", { class: "err" }, d.error ?? "error")); return; }
    const r = d.session;
    const calls = d.calls.map((c) => {
      const out = h("pre", { class: "out", hidden: "" }, c.output || "(empty)",
        c.output_bytes > 4096 ? h("div", { class: "muted" }, `\n… cut at 4 KB, ${c.output_bytes.toLocaleString()} bytes in total`) : null);
      const row = h("div", { class: `call${c.is_error ? " iserr" : ""}`, onclick: () => out.toggleAttribute("hidden") },
        h("span", { class: "seq" }, String(c.seq + 1)), chip(c.group, c.need ?? c.group), h("span", { class: "tool" }, c.tool ?? "?"),
        h("span", { class: "args" }, argLine(c.tool, c.input_json)), h("span", { class: "muted size" }, `${c.output_bytes.toLocaleString()} B${c.is_error ? " · error" : ""}`));
      return h("div", {}, row, out);
    });
    detail.replaceChildren(...nodes([
      h("div", { class: "line" }, h("span", { class: "tag" }, r.category ?? "-"), h("strong", {}, r.repo ?? "?"), h("span", { class: "muted" }, `${r.id}${r.sha ? ` @ ${r.sha.slice(0, 8)}` : ""} · ${r.mode ?? ""}`)),
      h("div", { class: "prompt-box" }, r.private ? h("span", { class: "muted" }, "private (history prompts and answers are not shown)") : (r.prompt ?? "")),
      d.final_answer ? h("details", { class: "box" }, h("summary", {}, "final answer"), h("div", { class: "answer" }, d.final_answer)) : null,
      h("div", { class: "strip big", style: `background:${stripBackground(d.calls.map((c) => Object.entries(CODES).find(([, g]) => g === c.group)![0]).join(""))}` }),
      h("div", { class: "muted" }, `${d.calls.length} calls · click a row for its output`),
      ...calls,
      d.diff ? h("div", { class: "box" }, h("h3", {}, "diff"), h("pre", { class: "diff" }, ...d.diff.split("\n").map((l) =>
        h("span", { class: l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : "" }, `${l}\n`)))) : null]));
  };

  const refresh = async () => { await Promise.all([loadSummary(), loadList()]); };
  void refresh();
};
